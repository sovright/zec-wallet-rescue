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
