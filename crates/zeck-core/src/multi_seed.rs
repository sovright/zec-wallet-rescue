//! Multi-seed scan resolver.
//!
//! Takes a list of [`SeedEntry`] values, derives fingerprints + accounts for
//! each, rejects exact-fingerprint duplicates, fills in missing birthdays via
//! [`BirthdayDetector`] (with a Sapling-activation fallback), and returns the
//! result sorted by birthday ascending.
//!
//! No scan logic lives here — this module is pure setup. The orchestrator that
//! consumes [`ResolvedSeed`]s arrives in a later task.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString, SecretVec};
use zip32::fingerprint::SeedFingerprint;

use crate::{
    birthday::detect_birthday,
    derivation::{derive_accounts, mnemonic_seed},
    models::{BirthdayDetectResult, DerivedAccount, ZeckNetwork},
    workspace::find_existing_workspace,
};

/// Sapling NU activation height on mainnet. Matches `zcash_protocol`'s
/// `MAIN_NETWORK` Sapling activation; pinned here so the fallback is a
/// const rather than a chain-info round-trip.
pub const MAINNET_SAPLING_ACTIVATION_HEIGHT: u32 = 419_200;
/// Sapling NU activation height on testnet.
pub const TESTNET_SAPLING_ACTIVATION_HEIGHT: u32 = 280_000;

fn sapling_activation_for(network: ZeckNetwork) -> u32 {
    match network {
        ZeckNetwork::Mainnet => MAINNET_SAPLING_ACTIVATION_HEIGHT,
        ZeckNetwork::Testnet => TESTNET_SAPLING_ACTIVATION_HEIGHT,
    }
}

/// One seed in a multi-seed batch. `birthday: None` triggers auto-detection.
pub struct SeedEntry {
    pub phrase: SecretString,
    pub birthday: Option<u32>,
    pub label: Option<String>,
}

/// A successfully resolved seed, ready for the orchestrator.
///
/// `index` is the **post-sort** position (0 = lowest birthday). Errors and
/// warnings emitted by [`resolve_seeds`] also reference post-sort indexes
/// when they are produced *after* sorting; pre-sort errors (invalid phrase,
/// duplicate fingerprint) reference the original input index instead — see
/// [`ResolveError`] for per-variant semantics.
pub struct ResolvedSeed {
    pub index: usize,
    // (Custom `Debug` impl below redacts `seed_bytes`.)
    /// Lowercase hex (64 chars) of the 32-byte ZIP-32 seed fingerprint.
    pub fingerprint: String,
    pub label: Option<String>,
    pub birthday: u32,
    pub seed_bytes: SecretVec<u8>,
    pub accounts: Vec<DerivedAccount>,
}

impl std::fmt::Debug for ResolvedSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedSeed")
            .field("index", &self.index)
            .field("fingerprint", &self.fingerprint)
            .field("label", &self.label)
            .field("birthday", &self.birthday)
            .field("seed_bytes", &"<redacted>")
            .field("accounts", &self.accounts)
            .finish()
    }
}

/// Error returned by [`resolve_seeds`].
#[derive(Debug)]
pub enum ResolveError {
    /// Phrase failed BIP-39 validation. `index` is the **original input** index.
    InvalidPhrase { index: usize, msg: String },
    /// Two or more entries derived the same seed fingerprint. `indexes` are the
    /// **original input** indexes that collided. The first colliding pair
    /// short-circuits the resolver.
    DuplicateFingerprint {
        indexes: Vec<usize>,
        fingerprint: String,
    },
    /// Birthday auto-detection failed and the Sapling-activation fallback also
    /// could not be determined. In practice this should never fire because the
    /// fallback is a const for both networks; kept for forward compatibility
    /// with future network variants. `index` is the **original input** index.
    BirthdayDetectionFailed { index: usize, msg: String },
}

/// Non-fatal warning emitted during resolution.
///
/// Indexes refer to the **post-sort** position in the returned vec, matching
/// [`ResolvedSeed::index`], so UI consumers can map warnings to rows directly.
#[derive(Debug)]
pub enum ResolveWarning {
    BirthdayDetectionFellBack {
        index: usize,
        fallback_height: u32,
        reason: String,
    },
    /// Reserved for the resume-detection pass (Task 10). Not populated here.
    // TODO(task-10): populate from workspace inspection.
    /// An existing workspace was found for this seed (matching fingerprint
    /// and network). Its stored birthday was used as authoritative — any
    /// user-supplied or auto-detected birthday is ignored so the workspace
    /// keying matches and resume works.
    ResumingExisting {
        index: usize,
        height: u32,
    },
}

/// Inputs shared across all entries.
pub struct ResolveConfig {
    pub network: ZeckNetwork,
    pub lightwalletd_url: String,
    pub data_dir: PathBuf,
    pub gap_limit: u32,
    pub num_accounts: Option<u32>,
}

/// Pluggable birthday detector so tests can stub out network I/O.
#[async_trait]
pub trait BirthdayDetector: Send + Sync {
    async fn detect(
        &self,
        seed_phrase: &SecretString,
        network: ZeckNetwork,
        lightwalletd_url: &str,
    ) -> Result<BirthdayDetectResult, String>;
}

/// Default detector that calls [`detect_birthday`] against a real lightwalletd.
pub struct DefaultLightwalletdDetector;

#[async_trait]
impl BirthdayDetector for DefaultLightwalletdDetector {
    async fn detect(
        &self,
        seed_phrase: &SecretString,
        network: ZeckNetwork,
        lightwalletd_url: &str,
    ) -> Result<BirthdayDetectResult, String> {
        detect_birthday(seed_phrase, network, lightwalletd_url, |_| {})
            .await
            .map_err(|err| err.to_string())
    }
}

/// Resolve a batch of seed entries. See module docs for the full contract.
///
/// The default detector (real lightwalletd) is used. Tests that want to stub
/// detection should use [`resolve_seeds_with_detector`] instead.
pub async fn resolve_seeds(
    entries: Vec<SeedEntry>,
    config: &ResolveConfig,
) -> Result<(Vec<ResolvedSeed>, Vec<ResolveWarning>), ResolveError> {
    resolve_seeds_with_detector(entries, config, Arc::new(DefaultLightwalletdDetector)).await
}

/// Variant of [`resolve_seeds`] taking an injectable detector for tests.
pub async fn resolve_seeds_with_detector(
    entries: Vec<SeedEntry>,
    config: &ResolveConfig,
    detector: Arc<dyn BirthdayDetector>,
) -> Result<(Vec<ResolvedSeed>, Vec<ResolveWarning>), ResolveError> {
    // ── Step 1: derive seeds, accounts, fingerprints (preserve input index) ──
    struct Pending {
        original_index: usize,
        fingerprint: String,
        label: Option<String>,
        phrase: SecretString,
        birthday: Option<u32>,
        seed_bytes: SecretVec<u8>,
        accounts: Vec<DerivedAccount>,
    }

    let account_count = config.num_accounts.unwrap_or(config.gap_limit).max(1);

    let mut pending: Vec<Pending> = Vec::with_capacity(entries.len());
    for (original_index, entry) in entries.into_iter().enumerate() {
        // mnemonic_seed validates the phrase as a side effect.
        let seed = mnemonic_seed(&entry.phrase).map_err(|err| ResolveError::InvalidPhrase {
            index: original_index,
            msg: err.to_string(),
        })?;

        let accounts = derive_accounts(&entry.phrase, config.network, account_count).map_err(
            |err| ResolveError::InvalidPhrase {
                index: original_index,
                msg: err.to_string(),
            },
        )?;

        let fingerprint = SeedFingerprint::from_seed(&seed)
            .ok_or_else(|| ResolveError::InvalidPhrase {
                index: original_index,
                msg: "seed length out of ZIP-32 range".to_owned(),
            })?
            .to_bytes();
        let fingerprint_hex = hex_lower(&fingerprint);

        pending.push(Pending {
            original_index,
            fingerprint: fingerprint_hex,
            label: entry.label,
            phrase: SecretString::new(entry.phrase.expose_secret().to_owned()),
            birthday: entry.birthday,
            seed_bytes: SecretVec::new(seed.to_vec()),
            accounts,
        });
    }

    // ── Step 2: dedup by fingerprint ─────────────────────────────────────────
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    for p in &pending {
        groups
            .entry(p.fingerprint.as_str())
            .or_default()
            .push(p.original_index);
    }
    // Find the first collision (lowest first-original-index) for determinism.
    if let Some((fp, idxs)) = pending
        .iter()
        .find_map(|p| {
            let g = groups.get(p.fingerprint.as_str())?;
            if g.len() > 1 && g[0] == p.original_index {
                Some((p.fingerprint.clone(), g.clone()))
            } else {
                None
            }
        })
    {
        return Err(ResolveError::DuplicateFingerprint {
            indexes: idxs,
            fingerprint: fp,
        });
    }

    // ── Step 3: fill in birthdays (auto-detect with fallback) ────────────────
    let fallback = sapling_activation_for(config.network);

    // Track warnings keyed by original_index; we'll remap to post-sort below.
    let mut pre_sort_warnings: Vec<(usize, ResolveWarning)> = Vec::new();

    let mut resolved_birthdays: Vec<u32> = Vec::with_capacity(pending.len());
    for p in &pending {
        // Resume override: if a workspace already exists on disk for this
        // (data_dir, network, fingerprint), its stored birthday is authoritative
        // — overriding both user-supplied and auto-detected values so the
        // workspace key still matches and resume works.
        if let Some((_root, meta)) =
            find_existing_workspace(&config.data_dir, config.network, &p.fingerprint)
        {
            resolved_birthdays.push(meta.birthday);
            pre_sort_warnings.push((
                p.original_index,
                ResolveWarning::ResumingExisting {
                    index: 0, // remapped after sort
                    height: meta.birthday,
                },
            ));
            continue;
        }

        if let Some(b) = p.birthday {
            resolved_birthdays.push(b);
            continue;
        }
        match detector
            .detect(&p.phrase, config.network, &config.lightwalletd_url)
            .await
        {
            Ok(result) => resolved_birthdays.push(result.birthday),
            Err(reason) => {
                resolved_birthdays.push(fallback);
                pre_sort_warnings.push((
                    p.original_index,
                    ResolveWarning::BirthdayDetectionFellBack {
                        index: 0, // remapped after sort
                        fallback_height: fallback,
                        reason,
                    },
                ));
            }
        }
    }

    // ── Step 4: build resolved seeds, sort stably by birthday asc ────────────
    let mut resolved: Vec<ResolvedSeed> = pending
        .into_iter()
        .zip(resolved_birthdays.into_iter())
        .map(|(p, birthday)| ResolvedSeed {
            // Tag with original_index temporarily so we can remap warnings.
            // Overwritten below.
            index: p.original_index,
            fingerprint: p.fingerprint,
            label: p.label,
            birthday,
            seed_bytes: p.seed_bytes,
            accounts: p.accounts,
        })
        .collect();

    resolved.sort_by_key(|r| r.birthday);

    // Build mapping original_index -> post_sort_index.
    let mut orig_to_post: HashMap<usize, usize> = HashMap::new();
    for (post, r) in resolved.iter().enumerate() {
        orig_to_post.insert(r.index, post);
    }
    for r in resolved.iter_mut().enumerate() {
        let (post, item) = r;
        item.index = post;
    }

    // Remap warnings to post-sort indexes.
    let warnings: Vec<ResolveWarning> = pre_sort_warnings
        .into_iter()
        .map(|(orig, w)| match w {
            ResolveWarning::BirthdayDetectionFellBack {
                fallback_height,
                reason,
                ..
            } => ResolveWarning::BirthdayDetectionFellBack {
                index: orig_to_post.get(&orig).copied().unwrap_or(0),
                fallback_height,
                reason,
            },
            ResolveWarning::ResumingExisting { height, .. } => ResolveWarning::ResumingExisting {
                index: orig_to_post.get(&orig).copied().unwrap_or(0),
                height,
            },
        })
        .collect();

    Ok((resolved, warnings))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(nibble(b >> 4));
        s.push(nibble(b & 0x0f));
    }
    s
}

fn nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // BIP-39 test vector: 24× abandon + art (entropy 0x00…00).
    const SEED_A: &str = "abandon abandon abandon abandon abandon abandon abandon abandon \
        abandon abandon abandon abandon abandon abandon abandon abandon \
        abandon abandon abandon abandon abandon abandon abandon art";

    // BIP-39 test vector: entropy 0x8080…80 → "letter advice … bless".
    const SEED_B: &str = "letter advice cage absurd amount doctor acoustic avoid letter advice \
        cage absurd amount doctor acoustic avoid letter advice cage absurd \
        amount doctor acoustic bless";

    // BIP-39 test vector: entropy 0xffff…ff → "zoo zoo zoo … vote".
    const SEED_C: &str = "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo \
        zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo vote";

    fn cfg() -> ResolveConfig {
        ResolveConfig {
            network: ZeckNetwork::Mainnet,
            lightwalletd_url: "https://invalid.example:443".to_owned(),
            data_dir: std::env::temp_dir(),
            gap_limit: 1,
            num_accounts: Some(1),
        }
    }

    fn entry(phrase: &str, birthday: Option<u32>, label: Option<&str>) -> SeedEntry {
        SeedEntry {
            phrase: SecretString::new(phrase.to_owned()),
            birthday,
            label: label.map(str::to_owned),
        }
    }

    /// Detector that returns a canned birthday for any input.
    struct FixedDetector(u32);

    #[async_trait]
    impl BirthdayDetector for FixedDetector {
        async fn detect(
            &self,
            _seed_phrase: &SecretString,
            _network: ZeckNetwork,
            _lightwalletd_url: &str,
        ) -> Result<BirthdayDetectResult, String> {
            Ok(BirthdayDetectResult {
                birthday: self.0,
                method: "test".to_owned(),
                message: "canned".to_owned(),
            })
        }
    }

    /// Detector that always fails so the Sapling-activation fallback fires.
    struct FailingDetector(&'static str);

    #[async_trait]
    impl BirthdayDetector for FailingDetector {
        async fn detect(
            &self,
            _: &SecretString,
            _: ZeckNetwork,
            _: &str,
        ) -> Result<BirthdayDetectResult, String> {
            Err(self.0.to_owned())
        }
    }

    /// Detector returning per-call results from a queue.
    struct QueueDetector {
        results: Mutex<Vec<Result<u32, String>>>,
    }

    #[async_trait]
    impl BirthdayDetector for QueueDetector {
        async fn detect(
            &self,
            _: &SecretString,
            _: ZeckNetwork,
            _: &str,
        ) -> Result<BirthdayDetectResult, String> {
            let mut q = self.results.lock().unwrap();
            match q.remove(0) {
                Ok(b) => Ok(BirthdayDetectResult {
                    birthday: b,
                    method: "test".to_owned(),
                    message: "queued".to_owned(),
                }),
                Err(e) => Err(e),
            }
        }
    }

    #[tokio::test]
    async fn resolver_rejects_duplicate_fingerprints() {
        let entries = vec![
            entry(SEED_A, Some(500_000), Some("first")),
            entry(SEED_B, Some(600_000), Some("middle")),
            entry(SEED_A, Some(700_000), Some("dupe")),
        ];
        let err = resolve_seeds_with_detector(entries, &cfg(), Arc::new(FixedDetector(500_000)))
            .await
            .unwrap_err();
        match err {
            ResolveError::DuplicateFingerprint { indexes, .. } => {
                assert_eq!(indexes, vec![0, 2]);
            }
            other => panic!("expected DuplicateFingerprint, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolver_sorts_by_birthday_ascending() {
        let entries = vec![
            entry(SEED_A, Some(2_500_000), None),
            entry(SEED_B, Some(500_000), None),
            entry(SEED_C, Some(1_000_000), None),
        ];
        let (resolved, warnings) =
            resolve_seeds_with_detector(entries, &cfg(), Arc::new(FixedDetector(0)))
                .await
                .unwrap();
        assert!(warnings.is_empty());
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0].birthday, 500_000);
        assert_eq!(resolved[1].birthday, 1_000_000);
        assert_eq!(resolved[2].birthday, 2_500_000);
    }

    #[tokio::test]
    async fn resolver_falls_back_to_sapling_activation_on_detection_failure() {
        let entries = vec![entry(SEED_A, None, None)];
        let (resolved, warnings) = resolve_seeds_with_detector(
            entries,
            &cfg(),
            Arc::new(FailingDetector("network down")),
        )
        .await
        .unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].birthday, MAINNET_SAPLING_ACTIVATION_HEIGHT);
        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            ResolveWarning::BirthdayDetectionFellBack {
                index,
                fallback_height,
                reason,
            } => {
                assert_eq!(*index, 0);
                assert_eq!(*fallback_height, MAINNET_SAPLING_ACTIVATION_HEIGHT);
                assert_eq!(reason, "network down");
            }
            other => panic!("expected BirthdayDetectionFellBack, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolver_falls_back_to_testnet_sapling_activation() {
        let mut config = cfg();
        config.network = ZeckNetwork::Testnet;
        let entries = vec![entry(SEED_A, None, None)];
        let (resolved, warnings) =
            resolve_seeds_with_detector(entries, &config, Arc::new(FailingDetector("nope")))
                .await
                .unwrap();
        assert_eq!(resolved[0].birthday, TESTNET_SAPLING_ACTIVATION_HEIGHT);
        assert!(matches!(
            warnings[0],
            ResolveWarning::BirthdayDetectionFellBack {
                fallback_height: TESTNET_SAPLING_ACTIVATION_HEIGHT,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn resolver_invalid_phrase_returns_error_with_correct_index() {
        let entries = vec![
            entry(SEED_A, Some(500_000), None),
            entry("not a real bip39 phrase at all", Some(500_000), None),
            entry(SEED_B, Some(500_000), None),
        ];
        let err = resolve_seeds_with_detector(entries, &cfg(), Arc::new(FixedDetector(0)))
            .await
            .unwrap_err();
        match err {
            ResolveError::InvalidPhrase { index, .. } => assert_eq!(index, 1),
            other => panic!("expected InvalidPhrase, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolver_post_sort_index_is_assigned_correctly() {
        // Entries with detection results queued in input order so we know the
        // post-sort mapping precisely.
        let queue = QueueDetector {
            results: Mutex::new(vec![
                Ok(3_000_000), // seed A → highest
                Err("offline".to_owned()), // seed B → fallback (lowest on mainnet)
                Ok(1_500_000), // seed C → middle
            ]),
        };
        let entries = vec![
            entry(SEED_A, None, Some("a")),
            entry(SEED_B, None, Some("b")),
            entry(SEED_C, None, Some("c")),
        ];
        let (resolved, warnings) =
            resolve_seeds_with_detector(entries, &cfg(), Arc::new(queue))
                .await
                .unwrap();

        // Post-sort: B(fallback=419200), C(1.5M), A(3M)
        assert_eq!(resolved[0].label.as_deref(), Some("b"));
        assert_eq!(resolved[0].index, 0);
        assert_eq!(resolved[0].birthday, MAINNET_SAPLING_ACTIVATION_HEIGHT);
        assert_eq!(resolved[1].label.as_deref(), Some("c"));
        assert_eq!(resolved[1].index, 1);
        assert_eq!(resolved[2].label.as_deref(), Some("a"));
        assert_eq!(resolved[2].index, 2);

        // The lone warning should reference post-sort index 0 (B).
        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            ResolveWarning::BirthdayDetectionFellBack { index, .. } => {
                assert_eq!(*index, 0);
            }
            other => panic!("expected BirthdayDetectionFellBack, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolver_preserves_stable_order_on_birthday_ties() {
        let entries = vec![
            entry(SEED_A, Some(500_000), Some("a")),
            entry(SEED_B, Some(500_000), Some("b")),
            entry(SEED_C, Some(500_000), Some("c")),
        ];
        let (resolved, _) = resolve_seeds_with_detector(entries, &cfg(), Arc::new(FixedDetector(0)))
            .await
            .unwrap();
        assert_eq!(resolved[0].label.as_deref(), Some("a"));
        assert_eq!(resolved[1].label.as_deref(), Some("b"));
        assert_eq!(resolved[2].label.as_deref(), Some("c"));
    }

    #[test]
    fn hex_lower_is_lowercase_and_no_prefix() {
        assert_eq!(hex_lower(&[0x00, 0xff, 0xab]), "00ffab");
        assert_eq!(hex_lower(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    /// Compute the resolver-format fingerprint for a phrase, matching the
    /// hex-encoded ZIP-32 seed fingerprint used both in resolved seeds and
    /// in `meta.json`. Mirrors the inline derivation in `resolve_seeds`.
    fn fingerprint_for_phrase(phrase: &str) -> String {
        let secret = SecretString::new(phrase.to_owned());
        let seed = mnemonic_seed(&secret).unwrap();
        let fp = SeedFingerprint::from_seed(&seed).unwrap().to_bytes();
        hex_lower(&fp)
    }

    /// Write a synthetic workspace dir matching the on-disk layout
    /// (`data_dir/<network>/<seed-fp>/birthday-<N>/<scope>/meta.json`) so the
    /// resolver's `find_existing_workspace` lookup succeeds without running
    /// the real `RecoveryWorkspace::initialize` (which would create SQLite
    /// databases and add I/O cost to the test).
    fn write_fake_workspace(
        data_dir: &std::path::Path,
        network: ZeckNetwork,
        fingerprint_hex: &str,
        birthday: u32,
        gap_limit: u32,
        num_accounts: Option<u32>,
    ) {
        // We don't need to mirror the upstream zip32 display string — the
        // resolver only opens `meta.json` and matches its `fingerprint` field,
        // so any directory name under the network bucket works.
        let scope = match num_accounts {
            Some(n) => format!("accounts-{n}"),
            None => format!("auto-gap-{gap_limit}"),
        };
        let root = data_dir
            .join(network.label())
            .join("synthetic-fp-dir")
            .join(format!("birthday-{birthday}"))
            .join(scope);
        std::fs::create_dir_all(&root).unwrap();
        let meta = crate::workspace::WorkspaceMeta {
            fingerprint: fingerprint_hex.to_owned(),
            birthday,
            num_accounts,
            gap_limit,
            network,
            version: 1,
        };
        let bytes = serde_json::to_vec_pretty(&meta).unwrap();
        std::fs::write(root.join("meta.json"), bytes).unwrap();
    }

    #[tokio::test]
    async fn resolver_resumes_existing_workspace_overriding_user_birthday() {
        let tmp = tempfile::tempdir().unwrap();
        let fp = fingerprint_for_phrase(SEED_A);
        write_fake_workspace(tmp.path(), ZeckNetwork::Mainnet, &fp, 2_400_000, 1, Some(1));

        let mut config = cfg();
        config.data_dir = tmp.path().to_path_buf();

        // User supplied a stale birthday that should be ignored.
        let entries = vec![entry(SEED_A, Some(9_999_999), Some("a"))];
        let (resolved, warnings) =
            resolve_seeds_with_detector(entries, &config, Arc::new(FixedDetector(123_456)))
                .await
                .unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].birthday, 2_400_000,
            "stored workspace birthday must override user-supplied value"
        );
        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            ResolveWarning::ResumingExisting { index, height } => {
                assert_eq!(*index, 0);
                assert_eq!(*height, 2_400_000);
            }
            other => panic!("expected ResumingExisting, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolver_resumes_existing_workspace_when_birthday_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let fp = fingerprint_for_phrase(SEED_A);
        write_fake_workspace(tmp.path(), ZeckNetwork::Mainnet, &fp, 2_400_000, 1, Some(1));

        let mut config = cfg();
        config.data_dir = tmp.path().to_path_buf();

        // FailingDetector would fall back to Sapling activation if it ran.
        // The workspace match should short-circuit before that.
        let entries = vec![entry(SEED_A, None, None)];
        let (resolved, warnings) = resolve_seeds_with_detector(
            entries,
            &config,
            Arc::new(FailingDetector("should not be called")),
        )
        .await
        .unwrap();

        assert_eq!(resolved[0].birthday, 2_400_000);
        assert!(matches!(
            warnings[0],
            ResolveWarning::ResumingExisting { height: 2_400_000, .. }
        ));
        assert_eq!(warnings.len(), 1);
    }

    #[tokio::test]
    async fn resolver_does_not_resume_workspace_with_different_network() {
        // Workspace is on testnet; resolver runs on mainnet → no resume.
        let tmp = tempfile::tempdir().unwrap();
        let fp = fingerprint_for_phrase(SEED_A);
        write_fake_workspace(tmp.path(), ZeckNetwork::Testnet, &fp, 280_500, 1, Some(1));

        let mut config = cfg();
        config.network = ZeckNetwork::Mainnet;
        config.data_dir = tmp.path().to_path_buf();

        let entries = vec![entry(SEED_A, Some(700_000), None)];
        let (resolved, warnings) =
            resolve_seeds_with_detector(entries, &config, Arc::new(FixedDetector(0)))
                .await
                .unwrap();

        // No resume warning, user-supplied birthday is honored.
        assert_eq!(resolved[0].birthday, 700_000);
        assert!(
            !warnings
                .iter()
                .any(|w| matches!(w, ResolveWarning::ResumingExisting { .. })),
            "must not resume when only the testnet workspace exists"
        );
    }
}
