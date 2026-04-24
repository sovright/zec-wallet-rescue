use std::{
    fs,
    path::{Path, PathBuf},
};

use rand_core::OsRng;
use secrecy::SecretVec;
use sha2::{Digest, Sha256};
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
/// Resume keying: identical scan args MUST resolve to the same `root`
/// across runs. This module's tests pin that contract. The downstream
/// behavior — that re-running ZECK with the same args actually resumes
/// from `WalletSummary::fully_scanned_height` — depends on `WalletDb`
/// and `BlockDb` initializers being idempotent against existing on-disk
/// workspaces, which is an upstream contract from `zcash_client_sqlite`.
/// That contract is not pinned here; treat it as a thing to verify
/// during dependency bumps.
///
/// Privacy: the per-wallet path component is a SHA-256-derived hash of
/// `(domain, network, seed-fingerprint, birthday, scope)` rather than the
/// literal seed fingerprint string. An attacker with the seed can still
/// recompute the path, but local filesystem inspection no longer surfaces
/// the bech32 fingerprint directly.
#[derive(Debug, Clone)]
pub struct RecoveryWorkspace {
    root: PathBuf,
    /// First path component under `data_dir/<network>` that is private to
    /// this wallet — used to tighten permissions without touching the
    /// generic data_dir or network directories above it.
    private_root: PathBuf,
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

        let workspace_id = derive_workspace_id(config.network, &fingerprint, config.birthday, &scope);
        let private_root = config
            .data_dir
            .join(config.network.label())
            .join(format!("workspace-{workspace_id}"));
        let root = private_root
            .join(format!("birthday-{}", config.birthday))
            .join(&scope);

        Ok(Self {
            wallet_db_path: root.join("wallet.sqlite"),
            cache_db_path: root.join("cache.sqlite"),
            root,
            private_root,
        })
    }

    pub fn initialize(&self, network: ZeckNetwork, seed: &[u8; 64]) -> ZeckResult<()> {
        create_private_dir_all(&self.root)?;
        // recursive create only sets mode on newly-created dirs; explicitly
        // re-tighten every component from the wallet-private root down so
        // resumes don't quietly inherit looser perms set in a previous run.
        tighten_private_perms(&self.private_root, &self.root)?;

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
        set_private_file_permissions(&self.wallet_db_path)?;

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
        set_private_file_permissions(&self.cache_db_path)?;

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

fn derive_workspace_id(
    network: ZeckNetwork,
    fingerprint: &SeedFingerprint,
    birthday: u32,
    scope: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"zeck-workspace-v1\0");
    hasher.update(network.label().as_bytes());
    hasher.update(b"\0");
    hasher.update(fingerprint.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(birthday.to_le_bytes());
    hasher.update(b"\0");
    hasher.update(scope.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

fn create_private_dir_all(path: &Path) -> ZeckResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        // mode(0o700) only applies to dirs created during this call; pre-
        // existing parents keep their mode. We explicitly tighten the
        // wallet-private subtree separately via tighten_private_perms.
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|err| ZeckError::Storage(format!("creating {}: {err}", path.display())))?;
    }

    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
            .map_err(|err| ZeckError::Storage(format!("creating {}: {err}", path.display())))?;
    }

    Ok(())
}

#[allow(unused_variables)]
fn tighten_private_perms(private_root: &Path, leaf: &Path) -> ZeckResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut current = leaf.to_path_buf();
        loop {
            fs::set_permissions(&current, fs::Permissions::from_mode(0o700)).map_err(|err| {
                ZeckError::Storage(format!(
                    "setting private permissions on {}: {err}",
                    current.display()
                ))
            })?;
            if current == private_root {
                break;
            }
            match current.parent() {
                Some(parent) => current = parent.to_path_buf(),
                None => break,
            }
        }
    }

    Ok(())
}

fn set_private_file_permissions(path: &Path) -> ZeckResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|err| {
            ZeckError::Storage(format!(
                "setting private permissions on {}: {err}",
                path.display()
            ))
        })?;
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use secrecy::SecretString;
    use zip32::fingerprint::SeedFingerprint;

    use super::*;
    use crate::derivation::mnemonic_seed;
    use crate::models::RuntimeScanConfig;

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
    fn workspace_path_does_not_leak_seed_fingerprint() {
        let cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        let ws = RecoveryWorkspace::from_runtime(&cfg).unwrap();
        let path_str = ws.root().display().to_string();

        let seed = mnemonic_seed(&cfg.seed_phrase).expect("seed should derive");
        let fingerprint_str = SeedFingerprint::from_seed(&seed)
            .expect("seed fingerprint should derive")
            .to_string();

        assert!(
            !path_str.contains(&fingerprint_str),
            "workspace path must not contain the literal seed fingerprint"
        );
        assert!(
            path_str.contains("workspace-"),
            "workspace path should use the hash-prefixed segment"
        );
    }

    #[test]
    fn workspace_path_does_not_change_across_releases() {
        // Snapshot the keying so an accidental change to the path layout
        // shows up as a test failure rather than a silently-orphaned
        // workspace on every user's disk.
        const EXPECTED_WORKSPACE_ID_FOR_TEST_SEED: &str = "b5e2cf2baecd3446f65e96a40159123d";
        let cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        let ws = RecoveryWorkspace::from_runtime(&cfg).unwrap();
        let suffix = ws
            .root()
            .strip_prefix(&cfg.data_dir)
            .unwrap()
            .to_string_lossy()
            .to_string();
        // network / workspace-<hash> / birthday-N / auto-gap-M
        let parts: Vec<&str> = suffix.split('/').collect();
        assert_eq!(parts.len(), 4, "unexpected workspace path layout: {suffix}");
        assert_eq!(parts[0], "mainnet");
        assert_eq!(
            parts[1],
            format!("workspace-{EXPECTED_WORKSPACE_ID_FOR_TEST_SEED}"),
            "workspace id changed for the fixed test seed — if intentional, \
             update EXPECTED_WORKSPACE_ID_FOR_TEST_SEED, but be aware this \
             means every existing user's workspace dir is now orphaned"
        );
        assert_eq!(parts[2], "birthday-3280000");
        assert_eq!(parts[3], "auto-gap-20");
    }

    #[test]
    fn workspace_id_is_deterministic_and_distinct_across_inputs() {
        let cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        let seed = mnemonic_seed(&cfg.seed_phrase).unwrap();
        let fp = SeedFingerprint::from_seed(&seed).unwrap();
        let a = derive_workspace_id(ZeckNetwork::Mainnet, &fp, 3_280_000, "auto-gap-20");
        let b = derive_workspace_id(ZeckNetwork::Mainnet, &fp, 3_280_000, "auto-gap-20");
        assert_eq!(a, b);
        let c = derive_workspace_id(ZeckNetwork::Mainnet, &fp, 3_280_001, "auto-gap-20");
        assert_ne!(a, c);
        let d = derive_workspace_id(ZeckNetwork::Testnet, &fp, 3_280_000, "auto-gap-20");
        assert_ne!(a, d);
    }
}
