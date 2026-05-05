use std::{
    fs,
    path::{Path, PathBuf},
};

use rand_core::OsRng;
use secrecy::SecretVec;
use serde::{Deserialize, Serialize};
use zcash_client_sqlite::{
    chain::init::init_cache_database, util::SystemClock, wallet::init::init_wallet_db, BlockDb,
    WalletDb,
};
use zcash_protocol::consensus::Network;
use zip32::fingerprint::SeedFingerprint;

use crate::{
    derivation::mnemonic_seed,
    error::{ZeckError, ZeckResult},
    models::{RuntimeScanConfig, ZeckNetwork},
};

const META_FILENAME: &str = "meta.json";
const META_VERSION: u32 = 1;

/// Persistent metadata for a workspace, written on `initialize` and read by
/// the multi-seed resolver to detect existing workspaces and override the
/// user-supplied birthday so the workspace key matches and resume works.
///
/// `fingerprint` is the lowercase 64-char hex of the 32-byte ZIP-32 seed
/// fingerprint, matching the format used by the multi-seed resolver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMeta {
    pub fingerprint: String,
    pub birthday: u32,
    pub num_accounts: Option<u32>,
    pub gap_limit: u32,
    pub network: ZeckNetwork,
    pub version: u32,
}

/// On-disk location of a wallet workspace for one (network, seed, birthday,
/// gap-strategy) tuple.
///
/// Resume keying: identical scan args MUST resolve to the same `root`
/// across runs. This module's tests pin that contract. The downstream
/// behavior — that re-running ZECK with the same args actually resumes
/// from `WalletSummary::fully_scanned_height` — depends on `WalletDb`
/// and `BlockDb` initializers being idempotent against existing on-disk
/// workspaces, which is an upstream contract from `zcash_client_sqlite`.
/// That contract is not pinned here; treat it as a thing to verify
/// during dependency bumps.
///
/// Callers that change birthday or gap-limit between runs intentionally land
/// on a fresh sub-workspace, so old state is preserved rather than corrupted
/// by mismatched scan windows. Workspace path layout is pinned by tests in
/// this module; do not change without updating both.
#[derive(Debug, Clone)]
pub struct RecoveryWorkspace {
    root: PathBuf,
    wallet_db_path: PathBuf,
    cache_db_path: PathBuf,
    data_dir: PathBuf,
    network: ZeckNetwork,
    fingerprint_hex: String,
    birthday: u32,
    num_accounts: Option<u32>,
    gap_limit: u32,
}

impl RecoveryWorkspace {
    pub fn from_runtime(config: &RuntimeScanConfig) -> ZeckResult<Self> {
        let seed = mnemonic_seed(&config.seed_phrase)?;
        let fingerprint = SeedFingerprint::from_seed(&seed).ok_or_else(|| {
            ZeckError::Internal("mnemonic seed length is out of the ZIP 32 range".to_owned())
        })?;

        let scope = match config.num_accounts {
            Some(num_accounts) => format!("accounts-{num_accounts}"),
            None => format!("auto-gap-{}", config.gap_limit),
        };

        let root = config
            .data_dir
            .join(config.network.label())
            .join(fingerprint.to_string())
            .join(format!("birthday-{}", config.birthday))
            .join(scope);

        Ok(Self {
            wallet_db_path: root.join("wallet.sqlite"),
            cache_db_path: root.join("cache.sqlite"),
            root,
            data_dir: config.data_dir.clone(),
            network: config.network,
            fingerprint_hex: hex_lower(&fingerprint.to_bytes()),
            birthday: config.birthday,
            num_accounts: config.num_accounts,
            gap_limit: config.gap_limit,
        })
    }

    pub fn initialize(&self, network: ZeckNetwork, seed: &[u8; 64]) -> ZeckResult<()> {
        fs::create_dir_all(&self.root).map_err(|err| {
            ZeckError::Storage(format!("creating {}: {err}", self.root.display()))
        })?;

        let mut wallet_db = WalletDb::for_path(
            &self.wallet_db_path,
            consensus_network(network),
            SystemClock,
            OsRng,
        )
        .map_err(|err| {
            ZeckError::Storage(format!(
                "opening wallet database {}: {err}",
                self.wallet_db_path.display()
            ))
        })?;
        init_wallet_db(&mut wallet_db, Some(SecretVec::new(seed.to_vec()))).map_err(|err| {
            ZeckError::Wallet(format!(
                "initializing wallet database {}: {err}",
                self.wallet_db_path.display()
            ))
        })?;

        let cache_db = BlockDb::for_path(&self.cache_db_path).map_err(|err| {
            ZeckError::Storage(format!(
                "opening cache database {}: {err}",
                self.cache_db_path.display()
            ))
        })?;
        init_cache_database(&cache_db).map_err(|err| {
            ZeckError::Wallet(format!(
                "initializing cache database {}: {err}",
                self.cache_db_path.display()
            ))
        })?;

        let meta = WorkspaceMeta {
            fingerprint: self.fingerprint_hex.clone(),
            birthday: self.birthday,
            num_accounts: self.num_accounts,
            gap_limit: self.gap_limit,
            network: self.network,
            version: META_VERSION,
        };
        self.write_meta(&meta)?;

        Ok(())
    }

    pub fn meta_path(&self) -> PathBuf {
        self.root.join(META_FILENAME)
    }

    pub fn read_meta(&self) -> Option<WorkspaceMeta> {
        read_meta_at(&self.meta_path())
    }

    pub fn write_meta(&self, meta: &WorkspaceMeta) -> ZeckResult<()> {
        let bytes = serde_json::to_vec_pretty(meta)
            .map_err(|err| ZeckError::Storage(format!("serializing meta.json: {err}")))?;
        fs::write(self.meta_path(), bytes).map_err(|err| {
            ZeckError::Storage(format!(
                "writing {}: {err}",
                self.meta_path().display()
            ))
        })?;
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn wallet_db_path(&self) -> &Path {
        &self.wallet_db_path
    }

    pub fn cache_db_path(&self) -> &Path {
        &self.cache_db_path
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn network(&self) -> ZeckNetwork {
        self.network
    }
}

fn read_meta_at(path: &Path) -> Option<WorkspaceMeta> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Walk `data_dir` looking for an existing workspace whose `meta.json` matches
/// the given fingerprint and network. The on-disk layout is
/// `data_dir/<network>/<fingerprint-zip32>/birthday-<N>/<scope>/meta.json`,
/// so we descend into the network bucket and walk through fingerprint/birthday
/// /scope sub-dirs. Malformed or missing meta files are skipped silently.
///
/// Returns the workspace `root` (the dir containing `meta.json`) along with
/// the parsed meta. The first matching workspace is returned; callers should
/// not rely on a specific ordering when multiple matches exist.
pub fn find_existing_workspace(
    data_dir: &Path,
    network: ZeckNetwork,
    fingerprint_hex: &str,
) -> Option<(PathBuf, WorkspaceMeta)> {
    let network_dir = data_dir.join(network.label());
    let fp_iter = fs::read_dir(&network_dir).ok()?;
    for fp_entry in fp_iter.flatten() {
        let fp_path = fp_entry.path();
        if !fp_path.is_dir() {
            continue;
        }
        let bday_iter = match fs::read_dir(&fp_path) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for bday_entry in bday_iter.flatten() {
            let bday_path = bday_entry.path();
            if !bday_path.is_dir() {
                continue;
            }
            let scope_iter = match fs::read_dir(&bday_path) {
                Ok(it) => it,
                Err(_) => continue,
            };
            for scope_entry in scope_iter.flatten() {
                let scope_path = scope_entry.path();
                if !scope_path.is_dir() {
                    continue;
                }
                let meta_path = scope_path.join(META_FILENAME);
                let Some(meta) = read_meta_at(&meta_path) else {
                    continue;
                };
                if meta.fingerprint == fingerprint_hex && meta.network == network {
                    return Some((scope_path, meta));
                }
            }
        }
    }
    None
}

pub fn network_cache_dir(data_dir: &Path, network: ZeckNetwork) -> PathBuf {
    data_dir.join("cache").join(network.label())
}

pub fn network_cache_db_path(data_dir: &Path, network: ZeckNetwork) -> PathBuf {
    network_cache_dir(data_dir, network).join("blocks.sqlite")
}

pub fn network_cache_lock_path(data_dir: &Path, network: ZeckNetwork) -> PathBuf {
    network_cache_dir(data_dir, network).join("blocks.lock")
}

pub fn consensus_network(network: ZeckNetwork) -> Network {
    match network {
        ZeckNetwork::Mainnet => Network::MainNetwork,
        ZeckNetwork::Testnet => Network::TestNetwork,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use secrecy::SecretString;

    use super::*;
    use crate::models::RuntimeScanConfig;

    /// Resume only works if a re-run with identical args resolves to the same
    /// on-disk workspace as the previous run. These tests pin the keying
    /// invariants the resume promise depends on.
    fn config(
        seed: &str,
        birthday: u32,
        num_accounts: Option<u32>,
        gap_limit: u32,
        network: ZeckNetwork,
    ) -> RuntimeScanConfig {
        RuntimeScanConfig {
            seed_phrase: SecretString::new(seed.to_owned()),
            birthday,
            num_accounts,
            gap_limit,
            lightwalletd_url: "https://example.invalid:443".to_owned(),
            data_dir: PathBuf::from("/tmp/zeck-test-data"),
            network,
        }
    }

    const SEED: &str = "abandon abandon abandon abandon abandon abandon \
                        abandon abandon abandon abandon abandon abandon \
                        abandon abandon abandon abandon abandon abandon \
                        abandon abandon abandon abandon abandon art";

    const OTHER_SEED: &str = "legal winner thank year wave sausage worth \
                              useful legal winner thank year wave sausage \
                              worth useful legal winner thank year wave \
                              sausage worth title";

    #[test]
    fn identical_args_produce_identical_workspace_path() {
        let a = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        let b = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        assert_eq!(a.root(), b.root());
    }

    #[test]
    fn different_birthday_uses_different_workspace() {
        // Different birthday is a different scan window, so persisting under
        // the same dir would corrupt the wallet.sqlite scan_summary state.
        let a = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        let b = RecoveryWorkspace::from_runtime(&config(
            SEED,
            2_500_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn different_gap_limit_uses_different_workspace() {
        // Different gap_limit means a different number of derived accounts to
        // scan against — must not silently reuse the smaller workspace.
        let a = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        let b = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            40,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn explicit_num_accounts_uses_different_workspace_than_gap_limit() {
        let a = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        let b = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            Some(20),
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        assert_ne!(
            a.root(),
            b.root(),
            "auto-gap and explicit num-accounts use different scan strategies and \
             must persist to separate workspaces"
        );
    }

    #[test]
    fn different_network_uses_different_workspace() {
        let a = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        let b = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Testnet,
        ))
        .unwrap();
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn different_seed_uses_different_workspace() {
        // Without seed-fingerprint isolation, two users sharing a data_dir
        // could clobber each other's workspaces.
        let a = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        let b = RecoveryWorkspace::from_runtime(&config(
            OTHER_SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn workspace_root_lives_under_data_dir() {
        let cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        let ws = RecoveryWorkspace::from_runtime(&cfg).unwrap();
        assert!(
            ws.root().starts_with(&cfg.data_dir),
            "workspace root {} must live under data_dir {}",
            ws.root().display(),
            cfg.data_dir.display(),
        );
    }

    #[test]
    fn print_fingerprint_for_snapshot() {
        // Prints the expected SeedFingerprint string for the fixed test
        // SEED so the snapshot test below can pin it. Run with
        // `cargo test workspace::tests::print_fingerprint_for_snapshot
        // -- --nocapture` to update the SEED_FINGERPRINT constant when
        // the upstream algorithm or display format changes intentionally.
        let cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        let ws = RecoveryWorkspace::from_runtime(&cfg).unwrap();
        let suffix = ws
            .root()
            .strip_prefix(&cfg.data_dir)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let parts: Vec<&str> = suffix.split('/').collect();
        eprintln!("fingerprint = {}", parts[1]);
    }

    #[test]
    fn initialize_writes_meta_json() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        cfg.data_dir = tmp.path().to_path_buf();

        let ws = RecoveryWorkspace::from_runtime(&cfg).unwrap();
        let seed = mnemonic_seed(&cfg.seed_phrase).unwrap();
        ws.initialize(cfg.network, &seed).unwrap();

        let meta = ws
            .read_meta()
            .expect("meta.json must be written by initialize");
        assert_eq!(meta.birthday, 3_280_000);
        assert_eq!(meta.num_accounts, None);
        assert_eq!(meta.gap_limit, 20);
        assert_eq!(meta.network, ZeckNetwork::Mainnet);
        assert_eq!(meta.version, 1);
        assert_eq!(meta.fingerprint.len(), 64, "fingerprint must be 64-char hex");
        assert!(
            meta.fingerprint.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "fingerprint must be lowercase hex with no prefix"
        );
    }

    #[test]
    fn find_existing_workspace_locates_initialized_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        cfg.data_dir = tmp.path().to_path_buf();

        let ws = RecoveryWorkspace::from_runtime(&cfg).unwrap();
        let seed = mnemonic_seed(&cfg.seed_phrase).unwrap();
        ws.initialize(cfg.network, &seed).unwrap();

        let meta = ws.read_meta().unwrap();
        let found = find_existing_workspace(&cfg.data_dir, ZeckNetwork::Mainnet, &meta.fingerprint);
        assert!(found.is_some(), "must find the workspace just initialized");
        let (root, found_meta) = found.unwrap();
        assert_eq!(root, ws.root());
        assert_eq!(found_meta.birthday, 3_280_000);

        // Wrong network → no match.
        assert!(
            find_existing_workspace(&cfg.data_dir, ZeckNetwork::Testnet, &meta.fingerprint)
                .is_none()
        );
    }

    #[test]
    fn network_cache_dir_is_scoped_by_network() {
        use std::path::PathBuf;
        let root = PathBuf::from("/tmp/zeck-data");
        let mainnet = network_cache_dir(&root, ZeckNetwork::Mainnet);
        let testnet = network_cache_dir(&root, ZeckNetwork::Testnet);
        assert_eq!(mainnet, root.join("cache").join("mainnet"));
        assert_eq!(testnet, root.join("cache").join("testnet"));
        assert_ne!(mainnet, testnet);
    }

    #[test]
    fn network_cache_db_and_lock_paths_are_under_cache_dir() {
        use std::path::PathBuf;
        let root = PathBuf::from("/tmp/zeck-data");
        let dir = network_cache_dir(&root, ZeckNetwork::Mainnet);
        assert_eq!(network_cache_db_path(&root, ZeckNetwork::Mainnet), dir.join("blocks.sqlite"));
        assert_eq!(network_cache_lock_path(&root, ZeckNetwork::Mainnet), dir.join("blocks.lock"));
    }

    #[test]
    fn workspace_path_does_not_change_across_releases() {
        // Snapshot the keying so an accidental change to the path layout
        // shows up as a test failure rather than a silently-orphaned
        // workspace on every user's disk. The fingerprint segment is
        // pinned to the *exact* string, not just the HRP, so a change
        // in the upstream zip32::SeedFingerprint algorithm or display
        // format will flip this test (which would otherwise silently
        // orphan every existing user's workspace dir).
        const EXPECTED_FINGERPRINT_FOR_TEST_SEED: &str =
            "zip32seedfp1uc59thq5rxtjutv06dymwsx7dfna3nm0a2h7jr8j7dazx3zkdnxqqgyu24";
        let cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        let ws = RecoveryWorkspace::from_runtime(&cfg).unwrap();
        let suffix = ws
            .root()
            .strip_prefix(&cfg.data_dir)
            .unwrap()
            .to_string_lossy()
            .to_string();
        // network / seed-fp / birthday-N / auto-gap-M
        let parts: Vec<&str> = suffix.split('/').collect();
        assert_eq!(parts.len(), 4, "unexpected workspace path layout: {suffix}");
        assert_eq!(parts[0], "mainnet");
        assert_eq!(
            parts[1], EXPECTED_FINGERPRINT_FOR_TEST_SEED,
            "seed fingerprint changed for the fixed test seed — if intentional, \
             update EXPECTED_FINGERPRINT_FOR_TEST_SEED, but be aware this means \
             every existing user's workspace dir is now orphaned"
        );
        assert_eq!(parts[2], "birthday-3280000");
        assert_eq!(parts[3], "auto-gap-20");
    }
}
