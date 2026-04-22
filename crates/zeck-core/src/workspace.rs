use std::{
    fs,
    path::{Path, PathBuf},
};

use rand_core::OsRng;
use secrecy::SecretVec;
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

/// On-disk location of a wallet workspace for one (network, seed, birthday,
/// gap-strategy) tuple.
///
/// Resume invariant: identical scan args MUST resolve to the same `root`
/// across runs, and the `WalletDb` and `BlockDb` initializers MUST be
/// idempotent against an existing on-disk workspace. This is what makes a
/// scan safely interruptible — quitting mid-scan and re-running with the
/// same flags picks up where the previous run left off (specifically, from
/// `WalletSummary::fully_scanned_height`).
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
    fn workspace_path_does_not_change_across_releases() {
        // Snapshot the keying so an accidental change to the path layout
        // shows up as a test failure rather than a silently-orphaned
        // workspace on every user's disk.
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
        assert!(parts[1].starts_with("zip32seedfp"), "seed fingerprint prefix expected, got {}", parts[1]);
        assert_eq!(parts[2], "birthday-3280000");
        assert_eq!(parts[3], "auto-gap-20");
    }
}
