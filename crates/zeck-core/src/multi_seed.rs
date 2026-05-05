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
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rand_core::OsRng;
use secrecy::{ExposeSecret, SecretString, SecretVec};
use zcash_client_backend::{
    data_api::{wallet::ConfirmationsPolicy, AccountBirthday, WalletRead},
    proto::service::{
        compact_tx_streamer_client::CompactTxStreamerClient, BlockId,
    },
};
use zcash_client_sqlite::{util::SystemClock, WalletDb};
use zcash_protocol::consensus::BlockHeight;
use zip32::fingerprint::SeedFingerprint;

use crate::{
    birthday::detect_birthday,
    cache::{CacheOpenError, SharedCacheReader, SharedCacheWriter},
    derivation::{derive_accounts, mnemonic_seed},
    error::{ZeckError, ZeckResult},
    fetcher::{spawn_fetcher, CancellationToken, FetcherConfig, FetcherProgress},
    lightwalletd::probe_lightwalletd_endpoints,
    models::{
        AccountBalancePreview, BirthdayDetectResult, DerivedAccount, ScanDiscovery, ZeckNetwork,
    },
    scan::{append_new_discoveries, import_accounts_into_workspace, TrackedAccount},
    scanner::{spawn_scanner, ChainStateProvider, ScannerSpec, SeedProgress, SeedStatus},
    workspace::{
        consensus_network, find_existing_workspace, network_cache_db_path,
        network_cache_lock_path, RecoveryWorkspace,
    },
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
#[derive(Clone, Debug, serde::Serialize)]
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

// ─────────────────────────────────────────────────────────────────────────────
// Multi-seed orchestrator (Phase 3 integration milestone).
//
// `start_multi_seed_run` resolves seeds, opens a shared block cache, spawns
// one fetcher and N scanners, then a driver task that:
//   * polls each scanner's `SeedProgress` (and the wallet DB for new
//     discoveries),
//   * computes aggregate progress (`blocks_scanned`, `synced_to_height`,
//     `phase`),
//   * waits for all scanner tasks to complete,
//   * cancels the fetcher and scanners on user-cancel or fetcher failure.
//
// Per-seed scanner failure is contained — the orchestrator marks that seed
// `Failed(...)` in its `SeedProgress` and lets the remaining scanners run.
//
// This is the smallest end-to-end Rust API needed to drive a multi-seed scan;
// service/Tauri/CLI/GUI integration follow in Phases 4–6.
// ─────────────────────────────────────────────────────────────────────────────

const AGGREGATOR_TICK: std::time::Duration = std::time::Duration::from_millis(1_000);

/// Aggregator-level configuration (the orchestrator's view of `RuntimeScanConfig`,
/// minus the seed phrase which is per-seed).
pub struct MultiSeedConfig {
    pub network: ZeckNetwork,
    pub lightwalletd_url: String,
    pub data_dir: PathBuf,
    pub gap_limit: u32,
    pub num_accounts: Option<u32>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub enum MultiSeedPhase {
    Resolving,
    Scanning,
    Completed,
    Cancelled,
    Failed(String),
}

/// Snapshot of the orchestrator's aggregate progress.
///
/// `discoveries` is append-only across the run; each entry carries
/// `seed_index` + `seed_fingerprint` so consumers can route them per-row.
/// `per_seed` mirrors the latest `SeedProgress` from each scanner.
#[derive(Clone, Debug, serde::Serialize)]
pub struct MultiSeedProgress {
    pub phase: MultiSeedPhase,
    pub blocks_scanned: u64,
    #[serde(with = "crate::models::serde_block_height::option")]
    pub synced_to_height: Option<BlockHeight>,
    pub discoveries: Vec<ScanDiscovery>,
    pub per_seed: Vec<SeedProgress>,
    pub fetcher: FetcherProgress,
    pub warnings: Vec<ResolveWarning>,
}

impl Default for MultiSeedProgress {
    fn default() -> Self {
        Self {
            phase: MultiSeedPhase::Resolving,
            blocks_scanned: 0,
            synced_to_height: None,
            discoveries: Vec::new(),
            per_seed: Vec::new(),
            fetcher: FetcherProgress {
                downloaded_to_height: None,
                target_tip: None,
                retry_count: 0,
            },
            warnings: Vec::new(),
        }
    }
}

/// Per-seed context retained by [`MultiSeedRun`] for post-scan operations
/// (sweep, report). Cloned cheaply because [`RecoveryWorkspace`] is `Clone`,
/// `tracked` is a small vec, and `seed_bytes` is wrapped in `SecretVec`.
#[derive(Clone)]
pub struct SeedSweepContext {
    pub seed_index: usize,
    pub fingerprint: String,
    pub label: Option<String>,
    pub birthday: u32,
    pub workspace: RecoveryWorkspace,
    pub tracked: Vec<TrackedAccount>,
    pub seed_bytes: Arc<SecretVec<u8>>,
    pub network: ZeckNetwork,
    pub lightwalletd_url: String,
}

/// Run handle returned by [`start_multi_seed_run`]. Holds shared progress,
/// a cancel token, the driver task, and per-seed sweep contexts.
pub struct MultiSeedRun {
    pub progress: Arc<Mutex<MultiSeedProgress>>,
    pub cancel: CancellationToken,
    pub task: tokio::task::JoinHandle<()>,
    /// Per-seed contexts needed for sweep/report after scan completion.
    /// Empty for terminal-failed runs (resolve errors).
    pub sweep_contexts: Vec<SeedSweepContext>,
}

impl MultiSeedRun {
    /// Lock-and-clone the current progress snapshot.
    pub fn snapshot(&self) -> MultiSeedProgress {
        self.progress
            .lock()
            .map(|guard| (*guard).clone())
            .unwrap_or_default()
    }

    /// Request a clean shutdown. The driver task will propagate cancellation
    /// to the fetcher and all scanners and transition `phase` to `Cancelled`
    /// once they've drained.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Return per-seed sweep contexts. Empty for terminal-failed runs.
    pub fn seed_sweep_contexts(&self) -> &[SeedSweepContext] {
        &self.sweep_contexts
    }
}

// Per-seed bookkeeping retained by the driver task.
struct SeedRuntime {
    seed_index: usize,
    fingerprint: String,
    workspace: RecoveryWorkspace,
    tracked: Vec<TrackedAccount>,
    progress: Arc<Mutex<SeedProgress>>,
    task: tokio::task::JoinHandle<Result<(), crate::scanner::ScannerError>>,
    cancel: CancellationToken,
    /// Last block-height at which we polled discoveries; used to skip
    /// redundant DB reads when no new blocks were scanned since the prior
    /// tick.
    last_discovery_height: u64,
    /// Per-seed append-only discovery log; passed to
    /// [`append_new_discoveries`] each tick so dedupe is scoped to the seed
    /// (the run-level vec collides on `(account_index, pool)` across seeds
    /// and can't be used directly).
    discoveries_log: Vec<ScanDiscovery>,
}

/// Build a [`MultiSeedRun`] whose `phase` is already terminal — used to
/// surface resolve-time and cache-open errors via the same handle shape as
/// successful runs (so callers don't need to discriminate on Result vs Run).
fn make_terminal_run(progress: MultiSeedProgress) -> MultiSeedRun {
    let cancel = CancellationToken::new();
    let progress = Arc::new(Mutex::new(progress));
    // Spawn a no-op task that resolves immediately so `task.await` is valid
    // for callers that prefer to await termination uniformly.
    let task = tokio::spawn(async move {});
    MultiSeedRun {
        progress,
        cancel,
        task,
        sweep_contexts: Vec::new(),
    }
}

/// Start a multi-seed scan run. See module docs for the high-level flow.
///
/// Returns a [`MultiSeedRun`]. Resolve-time failures (invalid mnemonic,
/// duplicate fingerprint, cache locked) are surfaced via `phase = Failed(...)`
/// rather than as `Err`, so callers always have a handle to inspect.
pub async fn start_multi_seed_run(
    entries: Vec<SeedEntry>,
    config: MultiSeedConfig,
) -> ZeckResult<MultiSeedRun> {
    // ── Phase: Resolving ─────────────────────────────────────────────────────
    let resolve_config = ResolveConfig {
        network: config.network,
        lightwalletd_url: config.lightwalletd_url.clone(),
        data_dir: config.data_dir.clone(),
        gap_limit: config.gap_limit,
        num_accounts: config.num_accounts,
    };

    let (seeds, warnings) = match resolve_seeds(entries, &resolve_config).await {
        Ok(pair) => pair,
        Err(err) => {
            return Ok(make_terminal_run(MultiSeedProgress {
                phase: MultiSeedPhase::Failed(format!("resolve failed: {err:?}")),
                ..Default::default()
            }));
        }
    };

    if seeds.is_empty() {
        return Ok(make_terminal_run(MultiSeedProgress {
            phase: MultiSeedPhase::Failed("no seeds provided".to_owned()),
            warnings,
            ..Default::default()
        }));
    }

    // ── Open shared cache (writer reserves the per-network lock) ────────────
    let cache_db = network_cache_db_path(&config.data_dir, config.network);
    let cache_lock = network_cache_lock_path(&config.data_dir, config.network);
    let writer = match SharedCacheWriter::open(&cache_db, &cache_lock) {
        Ok(w) => w,
        Err(CacheOpenError::Locked) => return Err(ZeckError::ScanLocked),
        Err(other) => {
            return Err(ZeckError::Storage(format!(
                "opening shared block cache: {other}"
            )))
        }
    };

    // ── Per-seed: workspace + account import + start-height discovery ───────
    let consensus = consensus_network(config.network);
    let mut per_seed_setup: Vec<(ResolvedSeed, RecoveryWorkspace, Vec<TrackedAccount>, BlockHeight)> =
        Vec::with_capacity(seeds.len());
    let mut min_start: Option<u32> = None;

    // We need a lightwalletd client both for the fetcher and for per-seed
    // birthday treestate lookups (used to build `AccountBirthday` for
    // `import_account_hd`). One initial probe, cloned thereafter.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (mut probe_client, _endpoint, _resp) =
        probe_lightwalletd_endpoints(&config.lightwalletd_url).await?;

    for resolved in seeds.into_iter() {
        let seed_bytes_array = secret_to_seed_array(&resolved.seed_bytes)?;
        let workspace = RecoveryWorkspace::from_seed_bytes(
            &seed_bytes_array,
            config.network,
            config.data_dir.clone(),
            resolved.birthday,
            config.num_accounts,
            config.gap_limit,
        )?;
        workspace.initialize(config.network, &seed_bytes_array)?;

        // Migrate any legacy per-workspace cache into the shared cache.
        writer.migrate_from(workspace.cache_db_path()).map_err(|err| {
            ZeckError::Storage(format!(
                "migrating legacy cache {}: {err}",
                workspace.cache_db_path().display()
            ))
        })?;

        // Build AccountBirthday from lightwalletd treestate.
        let account_birthday =
            build_account_birthday(&mut probe_client, resolved.birthday).await?;
        let transparent_account = crate::derivation::legacy_transparent_account_key_from_seed(
            config.network,
            &seed_bytes_array,
        )?;

        let tracked = import_accounts_into_workspace(
            &workspace,
            config.network,
            &seed_bytes_array,
            &account_birthday,
            &transparent_account,
            &resolved.accounts,
        )?;

        // Resume cursor: max(birthday, fully_scanned_height + 1).
        let resume_start = read_resume_start(&workspace, &consensus, resolved.birthday)?;
        let scanner_birthday = BlockHeight::from_u32(resolved.birthday);
        min_start = Some(match min_start {
            Some(prev) => prev.min(u32::from(resume_start)),
            None => u32::from(resume_start),
        });
        per_seed_setup.push((resolved, workspace, tracked, scanner_birthday));
    }

    let start_height = BlockHeight::from_u32(min_start.unwrap_or(0));

    // ── Spawn fetcher (consumes a client clone + the writer) ────────────────
    let fetcher_handle = spawn_fetcher(
        probe_client.clone(),
        writer,
        FetcherConfig {
            start_height,
            lightwalletd_endpoints: config.lightwalletd_url.clone(),
        },
    );

    // ── Build a shared chain-state provider for all scanners ────────────────
    let chain_state_client = probe_client.clone();
    let chain_state_provider: ChainStateProvider = Arc::new(move |height: BlockHeight| {
        let mut c = chain_state_client.clone();
        Box::pin(async move {
            let resp = c
                .get_tree_state(BlockId {
                    height: u64::from(height),
                    hash: vec![],
                })
                .await
                .map_err(|err| err.to_string())?;
            resp.into_inner()
                .to_chain_state()
                .map_err(|err| format!("decoding tree state at {height}: {err}"))
        })
    });

    // ── Spawn N scanners (each gets its own cache reader + cancel token) ────
    let mut seed_runtimes: Vec<SeedRuntime> = Vec::with_capacity(per_seed_setup.len());
    let mut sweep_contexts: Vec<SeedSweepContext> = Vec::with_capacity(per_seed_setup.len());
    for (resolved, workspace, tracked, scanner_birthday) in per_seed_setup {
        let reader = SharedCacheReader::open(&cache_db).map_err(|err| {
            ZeckError::Storage(format!("opening shared cache reader: {err}"))
        })?;
        let scanner_cancel = CancellationToken::new();
        let spec = ScannerSpec {
            seed_index: resolved.index,
            seed_fingerprint: resolved.fingerprint.clone(),
            seed_label: resolved.label.clone(),
            birthday: scanner_birthday,
            workspace: workspace.clone(),
            seed_bytes: SecretVec::new(resolved.seed_bytes.expose_secret().to_vec()),
            accounts: resolved.accounts.clone(),
            gap_limit: config.gap_limit,
            network: consensus,
        };
        let handle = spawn_scanner(
            spec,
            fetcher_handle.available_height.clone(),
            reader,
            chain_state_provider.clone(),
            scanner_cancel.clone(),
        );

        let seed_bytes_arc = Arc::new(SecretVec::new(resolved.seed_bytes.expose_secret().to_vec()));
        sweep_contexts.push(SeedSweepContext {
            seed_index: resolved.index,
            fingerprint: resolved.fingerprint.clone(),
            label: resolved.label.clone(),
            birthday: resolved.birthday,
            workspace: workspace.clone(),
            tracked: tracked.clone(),
            seed_bytes: seed_bytes_arc,
            network: config.network,
            lightwalletd_url: config.lightwalletd_url.clone(),
        });

        seed_runtimes.push(SeedRuntime {
            seed_index: resolved.index,
            fingerprint: resolved.fingerprint.clone(),
            workspace,
            tracked,
            progress: handle.progress,
            task: handle.task,
            cancel: scanner_cancel,
            last_discovery_height: 0,
            discoveries_log: Vec::new(),
        });
    }

    // ── Build the run-level progress + cancel + driver task ─────────────────
    let run_cancel = CancellationToken::new();
    let progress = Arc::new(Mutex::new(MultiSeedProgress {
        phase: MultiSeedPhase::Scanning,
        warnings,
        per_seed: seed_runtimes
            .iter()
            .map(|r| r.progress.lock().map(|g| g.clone()).unwrap())
            .collect(),
        ..Default::default()
    }));

    let driver = spawn_driver(
        progress.clone(),
        run_cancel.clone(),
        fetcher_handle,
        seed_runtimes,
        config.network,
    );

    Ok(MultiSeedRun {
        progress,
        cancel: run_cancel,
        task: driver,
        sweep_contexts,
    })
}

/// Build the driver task that ticks every [`AGGREGATOR_TICK`], copies per-seed
/// progress, polls discoveries from each wallet DB, and signals completion.
fn spawn_driver(
    progress: Arc<Mutex<MultiSeedProgress>>,
    cancel: CancellationToken,
    fetcher_handle: crate::fetcher::FetcherHandle,
    mut seeds: Vec<SeedRuntime>,
    network: ZeckNetwork,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let fetcher_cancel = fetcher_handle.cancel.clone();
        let fetcher_available = fetcher_handle.available_height.clone();
        let mut fetcher_task = fetcher_handle.task;
        let mut fetcher_outcome: Option<
            Result<crate::fetcher::FetcherSummary, crate::fetcher::FetcherError>,
        > = None;

        loop {
            // Cancellation propagates immediately to fetcher and scanners.
            if cancel.is_cancelled() {
                fetcher_cancel.cancel();
                for s in &seeds {
                    s.cancel.cancel();
                }
            }

            // Poll fetcher for completion (non-blocking).
            if fetcher_outcome.is_none() {
                if let Some(outcome) = poll_fetcher_done(&mut fetcher_task).await {
                    fetcher_outcome = Some(outcome);
                    if let Some(Err(crate::fetcher::FetcherError::Transport(_))) =
                        fetcher_outcome.as_ref()
                    {
                        // Fetcher transport failure: cancel all scanners so they
                        // drain promptly. The scanner's own logic will mark the
                        // seed Failed when its watch channel closes before
                        // catch-up.
                        for s in &seeds {
                            s.cancel.cancel();
                        }
                    }
                }
            }

            // Snapshot per-seed progress + poll discoveries from each wallet DB.
            let mut new_discoveries: Vec<ScanDiscovery> = Vec::new();
            let mut max_synced: Option<BlockHeight> = None;
            let mut total_blocks: u64 = 0;
            let mut per_seed_snapshot = Vec::with_capacity(seeds.len());
            let mut all_done = true;

            for runtime in seeds.iter_mut() {
                let snap = runtime
                    .progress
                    .lock()
                    .map(|g| g.clone())
                    .ok()
                    .unwrap_or_else(|| SeedProgress {
                        seed_index: runtime.seed_index,
                        seed_fingerprint: runtime.fingerprint.clone(),
                        label: None,
                        birthday: BlockHeight::from_u32(0),
                        fully_scanned_height: None,
                        status: SeedStatus::Pending,
                        balance_zatoshis: None,
                    });
                if let Some(h) = snap.fully_scanned_height {
                    max_synced = Some(match max_synced {
                        Some(prev) => prev.max(h),
                        None => h,
                    });
                    let bday_u32 = u32::from(snap.birthday);
                    let synced_u32 = u32::from(h);
                    if synced_u32 >= bday_u32 {
                        total_blocks =
                            total_blocks.saturating_add(u64::from(synced_u32 - bday_u32));
                    }
                }
                let is_terminal = matches!(
                    snap.status,
                    SeedStatus::Done | SeedStatus::Cancelled | SeedStatus::Failed(_)
                );
                if !is_terminal {
                    all_done = false;
                }
                per_seed_snapshot.push(snap);

                // Poll wallet DB for new (account, pool) funded balances.
                if let Some(found) = poll_seed_discoveries(runtime, network) {
                    new_discoveries.extend(found);
                }
            }

            // Update aggregate progress.
            {
                let fetcher_height = *fetcher_available.borrow();
                if let Ok(mut guard) = progress.lock() {
                    guard.per_seed = per_seed_snapshot;
                    guard.blocks_scanned = total_blocks;
                    guard.synced_to_height = max_synced.or(fetcher_height);
                    guard.fetcher = FetcherProgress {
                        downloaded_to_height: fetcher_height,
                        target_tip: fetcher_height,
                        retry_count: 0,
                    };
                    if !new_discoveries.is_empty() {
                        guard.discoveries.append(&mut new_discoveries);
                    }
                    // Update phase if terminal.
                    if cancel.is_cancelled() && all_done {
                        guard.phase = MultiSeedPhase::Cancelled;
                    } else if all_done {
                        // If fetcher errored, mark Failed; else Completed.
                        match &fetcher_outcome {
                            Some(Err(err)) => {
                                guard.phase = MultiSeedPhase::Failed(err.to_string());
                            }
                            _ => {
                                // If any seed failed, surface that.
                                let failed_seed = guard
                                    .per_seed
                                    .iter()
                                    .find_map(|p| match &p.status {
                                        SeedStatus::Failed(msg) => {
                                            Some((p.seed_index, msg.clone()))
                                        }
                                        _ => None,
                                    });
                                guard.phase = match failed_seed {
                                    Some((idx, msg)) => MultiSeedPhase::Failed(format!(
                                        "seed {idx}: {msg}"
                                    )),
                                    None => MultiSeedPhase::Completed,
                                };
                            }
                        }
                    }
                }
            }

            if all_done {
                break;
            }
            tokio::time::sleep(AGGREGATOR_TICK).await;
        }

        // Drain scanner tasks (best-effort — they should already be done).
        for runtime in seeds.into_iter() {
            let _ = runtime.task.await;
        }
        // Drain fetcher if it hasn't already.
        if fetcher_outcome.is_none() {
            let _ = fetcher_task.await;
        }
    })
}

/// Poll the fetcher's `JoinHandle` without blocking. Returns the inner result
/// once the task has completed; otherwise `None`.
async fn poll_fetcher_done(
    task: &mut tokio::task::JoinHandle<
        Result<crate::fetcher::FetcherSummary, crate::fetcher::FetcherError>,
    >,
) -> Option<Result<crate::fetcher::FetcherSummary, crate::fetcher::FetcherError>> {
    if task.is_finished() {
        match tokio::time::timeout(std::time::Duration::from_millis(10), task).await {
            Ok(Ok(inner)) => Some(inner),
            Ok(Err(_join_err)) => Some(Err(crate::fetcher::FetcherError::Transport(
                "fetcher task panicked".to_owned(),
            ))),
            Err(_) => None,
        }
    } else {
        None
    }
}

/// Open the seed's wallet DB, build account previews from its tracked
/// receivers, and run [`append_new_discoveries`] against an internal log.
/// Returns only the *newly emitted* entries, each tagged with `seed_index` +
/// `seed_fingerprint` so the orchestrator can append them to the run-level
/// discovery vec without re-deduping.
fn poll_seed_discoveries(
    runtime: &mut SeedRuntime,
    network: ZeckNetwork,
) -> Option<Vec<ScanDiscovery>> {
    let wallet_db = WalletDb::for_path(
        runtime.workspace.wallet_db_path(),
        consensus_network(network),
        SystemClock,
        OsRng,
    )
    .ok()?;
    let summary = wallet_db
        .get_wallet_summary(ConfirmationsPolicy::MIN)
        .ok()
        .flatten()?;
    let scanned_height = u64::from(u32::from(summary.fully_scanned_height()));

    // Compute the per-seed total balance every tick (cheap — already have the
    // summary in hand) and write it to the shared progress so the sweep view
    // can render funded counts without a separate DB read.
    let total_balance: u64 = runtime
        .tracked
        .iter()
        .map(|tracked| {
            summary
                .account_balances()
                .get(&tracked.wallet_account_id)
                .map(|v| u64::from(v.total()))
                .unwrap_or(0)
        })
        .sum();
    if let Ok(mut guard) = runtime.progress.lock() {
        guard.balance_zatoshis = Some(total_balance);
    }

    if scanned_height == runtime.last_discovery_height {
        return None;
    }
    runtime.last_discovery_height = scanned_height;

    // Build minimal AccountBalancePreview rows: only fields used by
    // `append_new_discoveries` need to be populated authoritatively.
    let mut rows: Vec<AccountBalancePreview> = Vec::with_capacity(runtime.tracked.len());
    for tracked in &runtime.tracked {
        let balance = summary.account_balances().get(&tracked.wallet_account_id);
        let sapling_zatoshis = balance
            .map(|v| u64::from(v.sapling_balance().total()))
            .unwrap_or(0);
        let orchard_zatoshis = balance
            .map(|v| u64::from(v.orchard_balance().total()))
            .unwrap_or(0);
        let transparent_zatoshis = balance
            .map(|v| u64::from(v.unshielded_balance().total()))
            .unwrap_or(0);
        rows.push(AccountBalancePreview {
            account_index: tracked.derived.index,
            sapling_address: tracked.derived.sapling_address.clone(),
            unified_address: tracked.derived.unified_address.clone(),
            transparent_receive_address: tracked.derived.transparent_receive_address.clone(),
            transparent_change_address: tracked.derived.transparent_change_address.clone(),
            transparent_utxo_count: 0,
            sapling_zatoshis,
            orchard_zatoshis,
            transparent_zatoshis,
            total_zatoshis: sapling_zatoshis
                .saturating_add(orchard_zatoshis)
                .saturating_add(transparent_zatoshis),
            has_activity: false,
            status: String::new(),
        });
    }

    // Dedupe is scoped per-seed: `(account_index, pool)` collides across
    // seeds, so we keep a per-seed append-only log on `SeedRuntime` and only
    // return the *newly-appended* tail to the caller.
    let mut log = std::mem::take(&mut runtime.discoveries_log);
    let before = log.len();
    append_new_discoveries(&mut log, &rows, scanned_height);
    let added: Vec<ScanDiscovery> = log[before..]
        .iter()
        .map(|d| ScanDiscovery {
            seed_index: runtime.seed_index,
            seed_fingerprint: runtime.fingerprint.clone(),
            ..d.clone()
        })
        .collect();
    runtime.discoveries_log = log;
    if added.is_empty() {
        None
    } else {
        Some(added)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn secret_to_seed_array(secret: &SecretVec<u8>) -> ZeckResult<[u8; 64]> {
    let bytes = secret.expose_secret();
    if bytes.len() != 64 {
        return Err(ZeckError::Internal(format!(
            "expected 64-byte seed, got {}",
            bytes.len()
        )));
    }
    let mut arr = [0u8; 64];
    arr.copy_from_slice(bytes);
    Ok(arr)
}

async fn build_account_birthday(
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    birthday: u32,
) -> ZeckResult<AccountBirthday> {
    // Same approach as scan.rs: fetch the treestate at `birthday - 1` (so
    // the scan starts *at* `birthday` with the prior block's commitment
    // frontier) and convert via `AccountBirthday::from_treestate`.
    let prior = birthday.saturating_sub(1);
    let resp = client
        .get_tree_state(BlockId {
            height: u64::from(prior),
            hash: vec![],
        })
        .await
        .map_err(|err| ZeckError::Lightwalletd(err.to_string()))?
        .into_inner();
    AccountBirthday::from_treestate(resp, None)
        .map_err(|_| ZeckError::Wallet("constructing account birthday from treestate".to_owned()))
}

fn read_resume_start(
    workspace: &RecoveryWorkspace,
    network: &zcash_protocol::consensus::Network,
    birthday: u32,
) -> ZeckResult<BlockHeight> {
    let wallet_db =
        WalletDb::for_path(workspace.wallet_db_path(), *network, SystemClock, OsRng).map_err(
            |err| ZeckError::Storage(format!("opening wallet database: {err}")),
        )?;
    let fully_scanned = wallet_db
        .get_wallet_summary(ConfirmationsPolicy::MIN)
        .map_err(|err| ZeckError::Wallet(format!("loading wallet summary: {err}")))?
        .map(|s| s.fully_scanned_height());
    Ok(match fully_scanned {
        Some(h) if u32::from(h) >= birthday => BlockHeight::from_u32(u32::from(h).saturating_add(1)),
        _ => BlockHeight::from_u32(birthday),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ─── Orchestrator tests ──────────────────────────────────────────────────
    //
    // The full multi-seed end-to-end test (real lightwalletd + chain blocks)
    // is intentionally deferred to Task 21's integration test — driving
    // `scan_cached_blocks` against a synthetic compact-block stream requires
    // a fully-formed sapling commitment frontier, which is well outside the
    // surface this task introduces. The two tests below exercise the failure
    // paths that don't need real chain data.

    #[tokio::test]
    async fn multi_seed_run_failed_resolve_returns_failed_phase() {
        // Two entries with the same seed phrase → DuplicateFingerprint.
        let tmp = tempfile::tempdir().unwrap();
        let entries = vec![
            entry(SEED_A, Some(500_000), Some("first")),
            entry(SEED_A, Some(600_000), Some("dup")),
        ];
        let cfg = MultiSeedConfig {
            network: ZeckNetwork::Mainnet,
            // Invalid endpoint guarantees no network I/O happens before the
            // resolver short-circuits on the duplicate.
            lightwalletd_url: "https://invalid.example:443".to_owned(),
            data_dir: tmp.path().to_path_buf(),
            gap_limit: 1,
            num_accounts: Some(1),
        };
        let run = start_multi_seed_run(entries, cfg).await.unwrap();
        // The driver task on a terminal-failed run resolves immediately.
        let snap = run.snapshot();
        let _ = run.task.await;
        match snap.phase {
            MultiSeedPhase::Failed(msg) => {
                assert!(
                    msg.contains("resolve failed") || msg.contains("Duplicate"),
                    "unexpected failure message: {msg}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(snap.per_seed.is_empty());
        assert!(snap.discoveries.is_empty());
    }

    #[tokio::test]
    async fn multi_seed_run_no_entries_returns_failed_phase() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = MultiSeedConfig {
            network: ZeckNetwork::Mainnet,
            lightwalletd_url: "https://invalid.example:443".to_owned(),
            data_dir: tmp.path().to_path_buf(),
            gap_limit: 1,
            num_accounts: Some(1),
        };
        let run = start_multi_seed_run(Vec::new(), cfg).await.unwrap();
        let snap = run.snapshot();
        let _ = run.task.await;
        assert!(matches!(snap.phase, MultiSeedPhase::Failed(_)));
    }

    #[test]
    fn multi_seed_progress_default_starts_in_resolving_phase() {
        let p = MultiSeedProgress::default();
        assert!(matches!(p.phase, MultiSeedPhase::Resolving));
        assert_eq!(p.blocks_scanned, 0);
        assert!(p.synced_to_height.is_none());
        assert!(p.discoveries.is_empty());
    }

    #[tokio::test]
    async fn multi_seed_run_cancel_on_terminal_run_is_noop() {
        // After a resolve-time failure, the run is already terminal; calling
        // cancel must not panic and snapshot must remain Failed.
        let tmp = tempfile::tempdir().unwrap();
        let entries = vec![
            entry(SEED_A, Some(500_000), None),
            entry(SEED_A, Some(600_000), None),
        ];
        let cfg = MultiSeedConfig {
            network: ZeckNetwork::Mainnet,
            lightwalletd_url: "https://invalid.example:443".to_owned(),
            data_dir: tmp.path().to_path_buf(),
            gap_limit: 1,
            num_accounts: Some(1),
        };
        let run = start_multi_seed_run(entries, cfg).await.unwrap();
        run.cancel();
        let snap = run.snapshot();
        let _ = run.task.await;
        assert!(matches!(snap.phase, MultiSeedPhase::Failed(_)));
    }
}
