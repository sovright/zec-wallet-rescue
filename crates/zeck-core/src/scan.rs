use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex as StdMutex,
};

use async_trait::async_trait;
use prost::Message;
use rand_core::OsRng;
use rusqlite::{params, Connection, OptionalExtension};
use rustls::crypto::ring::default_provider;
use secrecy::SecretVec;
use tokio::sync::Mutex;
use tonic::{
    body::Body as TonicBody,
    client::GrpcService,
    codegen::{Body, Bytes, StdError},
};
use tracing::warn;
use zcash_client_backend::{
    data_api::{
        chain::{error::Error as ChainError, BlockCache, BlockSource},
        scanning::ScanRange,
        wallet::ConfirmationsPolicy,
        Account as _, AccountBirthday, InputSource, WalletRead, WalletWrite, Zip32Derivation,
    },
    proto::{
        compact_formats::CompactBlock,
        service::{
            compact_tx_streamer_client::CompactTxStreamerClient, BlockId, GetAddressUtxosArg,
        },
    },
    sync,
};
use zcash_client_sqlite::{util::SystemClock, AccountUuid, WalletDb};
use zcash_protocol::consensus::BlockHeight;
use zcash_transparent::address::TransparentAddress;
use zip32::{fingerprint::SeedFingerprint, AccountId};

use crate::{
    derivation::{
        derive_accounts, legacy_transparent_account_key, legacy_transparent_pubkey, mnemonic_seed,
    },
    error::{ZeckError, ZeckResult},
    lightwalletd::{build_probe, describe_lightwalletd_endpoints, probe_lightwalletd_endpoints},
    models::{
        AccountBalancePreview, AddressScope, DerivedAccount, DiscoveryPool, LightwalletdProbe,
        RuntimeScanConfig, ScanDiscovery, ScanHandle, ScanPhase, ScanProgress, ScanSummary,
    },
    workspace::{consensus_network, RecoveryWorkspace},
};

const MAX_ACCOUNT_SCAN_COUNT: u32 = 500;
const SYNC_BATCH_SIZE: u32 = 1_000;

#[derive(Debug)]
enum CacheError {
    Db(rusqlite::Error),
    Decode(prost::DecodeError),
    MissingBlock(BlockHeight),
    Corrupted(String),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(err) => write!(f, "{err}"),
            Self::Decode(err) => write!(f, "{err}"),
            Self::MissingBlock(height) => write!(f, "missing compact block at height {height}"),
            Self::Corrupted(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CacheError {}

impl From<rusqlite::Error> for CacheError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Db(value)
    }
}

impl From<prost::DecodeError> for CacheError {
    fn from(value: prost::DecodeError) -> Self {
        Self::Decode(value)
    }
}

struct SqliteBlockCache(StdMutex<Connection>);

impl SqliteBlockCache {
    fn for_path(path: &std::path::Path) -> Result<Self, CacheError> {
        Ok(Self(StdMutex::new(Connection::open(path)?)))
    }
}

impl BlockSource for SqliteBlockCache {
    type Error = CacheError;

    fn with_blocks<F, DbErrT>(
        &self,
        from_height: Option<BlockHeight>,
        limit: Option<usize>,
        mut with_row: F,
    ) -> Result<(), ChainError<DbErrT, Self::Error>>
    where
        F: FnMut(CompactBlock) -> Result<(), ChainError<DbErrT, Self::Error>>,
    {
        fn to_chain_error<DbErrT>(err: CacheError) -> ChainError<DbErrT, CacheError> {
            ChainError::BlockSource(err)
        }

        let start_height = from_height.map_or(0u32, u32::from);
        let row_limit = limit
            .and_then(|limit| u32::try_from(limit).ok())
            .unwrap_or(u32::MAX);
        let guard = self
            .0
            .lock()
            .map_err(|_| CacheError::Corrupted("block cache mutex was poisoned".to_owned()))
            .map_err(to_chain_error)?;
        let mut stmt = guard
            .prepare(
                "SELECT height, data FROM compactblocks
                 WHERE height >= ?
                 ORDER BY height ASC LIMIT ?",
            )
            .map_err(CacheError::from)
            .map_err(to_chain_error)?;
        let mut rows = stmt
            .query(params![start_height, row_limit])
            .map_err(CacheError::from)
            .map_err(to_chain_error)?;

        let mut expected = from_height;
        while let Some(row) = rows
            .next()
            .map_err(CacheError::from)
            .map_err(to_chain_error)?
        {
            let height = BlockHeight::from_u32(
                row.get::<_, u32>(0)
                    .map_err(CacheError::from)
                    .map_err(to_chain_error)?,
            );
            if let Some(expected_height) = expected {
                if height != expected_height {
                    return Err(to_chain_error(CacheError::MissingBlock(expected_height)));
                }
            }
            let data = row
                .get::<_, Vec<u8>>(1)
                .map_err(CacheError::from)
                .map_err(to_chain_error)?;
            let block = CompactBlock::decode(&data[..])
                .map_err(CacheError::from)
                .map_err(to_chain_error)?;
            if block.height() != height {
                return Err(to_chain_error(CacheError::Corrupted(format!(
                    "cached block height {} did not match row height {}",
                    block.height(),
                    height
                ))));
            }
            with_row(block)?;
            expected = expected.map(|height| height + 1);
        }

        if let Some(expected_height) = expected {
            if expected_height == from_height.unwrap_or(BlockHeight::from_u32(start_height)) {
                return Err(to_chain_error(CacheError::MissingBlock(expected_height)));
            }
        }

        Ok(())
    }
}

#[async_trait]
impl BlockCache for SqliteBlockCache {
    fn get_tip_height(
        &self,
        range: Option<&ScanRange>,
    ) -> Result<Option<BlockHeight>, Self::Error> {
        let (start_height, end_height) = range
            .map(|range: &ScanRange| {
                (
                    u32::from(range.block_range().start),
                    u32::from(range.block_range().end),
                )
            })
            .unwrap_or((0, u32::MAX));

        self.0
            .lock()
            .map_err(|_| CacheError::Corrupted("block cache mutex was poisoned".to_owned()))?
            .query_row(
                "SELECT MAX(height) FROM compactblocks WHERE height >= ? AND height < ?",
                params![start_height, end_height],
                |row| row.get::<_, Option<u32>>(0),
            )
            .map(|height| height.map(BlockHeight::from_u32))
            .map_err(CacheError::from)
    }

    async fn read(&self, range: &ScanRange) -> Result<Vec<CompactBlock>, Self::Error> {
        let mut blocks = Vec::new();
        let start = range.block_range().start;
        let end = range.block_range().end;
        let guard = self
            .0
            .lock()
            .map_err(|_| CacheError::Corrupted("block cache mutex was poisoned".to_owned()))?;
        let mut stmt = guard.prepare(
            "SELECT height, data FROM compactblocks
             WHERE height >= ? AND height < ?
             ORDER BY height ASC",
        )?;
        let mut rows = stmt.query(params![u32::from(start), u32::from(end)])?;
        let mut expected = start;

        while let Some(row) = rows.next()? {
            let height = BlockHeight::from_u32(row.get(0)?);
            if height != expected {
                if blocks.is_empty() {
                    return Err(CacheError::MissingBlock(expected));
                }
                break;
            }
            let data: Vec<u8> = row.get(1)?;
            let block = CompactBlock::decode(&data[..])?;
            blocks.push(block);
            expected = expected + 1;
        }

        Ok(blocks)
    }

    async fn insert(&self, compact_blocks: Vec<CompactBlock>) -> Result<(), Self::Error> {
        let guard = self
            .0
            .lock()
            .map_err(|_| CacheError::Corrupted("block cache mutex was poisoned".to_owned()))?;
        let mut stmt = guard.prepare(
            "INSERT INTO compactblocks(height, data)
             VALUES (?, ?)
             ON CONFLICT(height) DO UPDATE SET data = excluded.data",
        )?;
        guard.execute("BEGIN IMMEDIATE", [])?;
        let result = compact_blocks.iter().try_for_each(|block| {
            stmt.execute(params![u32::from(block.height()), block.encode_to_vec()])?;
            Ok::<_, rusqlite::Error>(())
        });
        drop(stmt);
        match result {
            Ok(()) => {
                guard.execute("COMMIT", [])?;
                Ok(())
            }
            Err(err) => {
                let _ = guard.execute("ROLLBACK", []);
                Err(CacheError::from(err))
            }
        }
    }

    async fn delete(&self, range: ScanRange) -> Result<(), Self::Error> {
        self.0
            .lock()
            .map_err(|_| CacheError::Corrupted("block cache mutex was poisoned".to_owned()))?
            .execute(
                "DELETE FROM compactblocks WHERE height >= ? AND height < ?",
                params![
                    u32::from(range.block_range().start),
                    u32::from(range.block_range().end)
                ],
            )?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct TrackedAccount {
    pub wallet_account_id: AccountUuid,
    pub derived: DerivedAccount,
    pub transparent_receivers: Vec<TransparentAddress>,
}

#[derive(Debug)]
pub struct ScanTaskState {
    pub progress: ScanProgress,
    pub cancelled: Arc<AtomicBool>,
    pub workspace: Option<RecoveryWorkspace>,
    pub tracked_accounts: Vec<TrackedAccount>,
}

impl ScanTaskState {
    pub fn new(handle: ScanHandle) -> Self {
        Self {
            progress: ScanProgress {
                handle,
                phase: ScanPhase::Idle,
                blocks_scanned: 0,
                blocks_total: 0,
                elapsed_seconds: None,
                estimated_remaining_seconds: None,
                accounts: vec![],
                discoveries: vec![],
                summary: None,
                server: None,
                message: None,
                error: None,
            },
            cancelled: Arc::new(AtomicBool::new(false)),
            workspace: None,
            tracked_accounts: vec![],
        }
    }
}

pub type SharedScanTaskState = Arc<Mutex<ScanTaskState>>;

pub async fn run_recovery_scan(state: SharedScanTaskState, config: RuntimeScanConfig) {
    match run_recovery_scan_inner(state.clone(), config).await {
        Ok(()) | Err(ZeckError::Cancelled) => {}
        Err(err) => {
            let mut guard = state.lock().await;
            guard.progress.phase = ScanPhase::Error;
            guard.progress.error = Some(err.to_string());
            guard.progress.message = Some(if guard.progress.accounts.is_empty() {
                "Recovery scan failed before any legacy addresses were derived.".to_owned()
            } else if guard.progress.server.is_none() {
                "Legacy addresses were derived locally, but lightwalletd probing failed before shielded recovery could begin."
                    .to_owned()
            } else {
                "Partial results are shown below, but the recovery scan ended before the wallet workspace finished syncing."
                    .to_owned()
            });
        }
    }
}

async fn run_recovery_scan_inner(
    state: SharedScanTaskState,
    config: RuntimeScanConfig,
) -> ZeckResult<()> {
    {
        let mut guard = state.lock().await;
        guard.progress.phase = ScanPhase::ValidatingSeed;
        guard.progress.message = Some("Validating BIP-39 seed phrase.".to_owned());
    }

    let seed = mnemonic_seed(&config.seed_phrase)?;
    let workspace = RecoveryWorkspace::from_runtime(&config)?;
    workspace.initialize(config.network, &seed)?;
    let transparent_account = legacy_transparent_account_key(&config.seed_phrase, config.network)?;

    {
        let mut guard = state.lock().await;
        guard.workspace = Some(workspace.clone());
    }

    let max_accounts = resolve_max_account_count(&config)?;
    let mut imported_accounts = 0u32;
    let mut target_accounts = initial_batch_size(&config, max_accounts);
    let network = consensus_network(config.network);
    let initial_accounts = derive_accounts(&config.seed_phrase, config.network, target_accounts)?;

    {
        let mut guard = state.lock().await;
        guard.progress.phase = ScanPhase::DerivingKeys;
        guard.progress.message = Some(format!(
            "Derived {target_accounts} ZecWallet Lite-compatible account slots locally. Checking lightwalletd next."
        ));
    }
    initialize_accounts(&state, &initial_accounts).await;

    let configured_endpoints = describe_lightwalletd_endpoints(&config.lightwalletd_url);

    {
        let mut guard = state.lock().await;
        guard.progress.phase = ScanPhase::ProbingLightwalletd;
        guard.progress.message = Some(format!(
            "Connecting to {configured_endpoints} and checking chain metadata.",
        ));
    }

    let _ = default_provider().install_default();
    let (mut client, endpoint, response) =
        probe_lightwalletd_endpoints(&config.lightwalletd_url).await?;
    let chain_tip_height = u32::try_from(response.block_height)
        .map_err(|_| ZeckError::Lightwalletd("chain tip height overflowed u32".to_owned()))?;
    let probe: LightwalletdProbe = build_probe(endpoint, &response);
    // Clamp birthday to sapling_activation_height + 1 so we never request a
    // pre-Sapling treestate (block 419199 and earlier have no Sapling tree).
    let sapling_floor = u32::try_from(response.sapling_activation_height)
        .unwrap_or(419_201)
        .saturating_add(1);
    let effective_birthday = config.birthday.max(sapling_floor).min(chain_tip_height);
    let birthday_treestate = client
        .get_tree_state(BlockId {
            height: u64::from(effective_birthday.saturating_sub(1)),
            hash: vec![],
        })
        .await
        .map_err(|err| ZeckError::Lightwalletd(err.to_string()))?
        .into_inner();
    let account_birthday = AccountBirthday::from_treestate(
        birthday_treestate,
        Some(BlockHeight::from_u32(chain_tip_height)),
    )
    .map_err(|_| ZeckError::Wallet("constructing account birthday from treestate".to_owned()))?;

    {
        let mut guard = state.lock().await;
        guard.progress.server = Some(probe);
        guard.progress.blocks_total = block_delta(chain_tip_height, effective_birthday);
    }

    // Fast transparent-only probe: lightwalletd's GetAddressUtxos answers in
    // milliseconds for a batch of t-addrs, so we can surface preliminary
    // transparent balances within seconds — long before the multi-hour
    // shielded compact-block scan finishes. This either (a) shows users
    // their funds immediately for transparent-only wallets, or (b) gives
    // them an early "yes, this seed has something" signal that justifies
    // the longer wait. Failures here are non-fatal; the shielded scan
    // below will still discover transparent UTXOs authoritatively.
    if let Err(err) =
        run_transparent_quick_probe(&state, &mut client, &initial_accounts, chain_tip_height).await
    {
        warn!("transparent quick probe failed (continuing with shielded scan): {err}");
    }

    while imported_accounts < target_accounts {
        check_cancelled(&state).await?;

        {
            let mut guard = state.lock().await;
            guard.progress.phase = ScanPhase::DerivingKeys;
            guard.progress.message = Some(format!(
                "Preparing legacy account slots 0 through {}.",
                target_accounts.saturating_sub(1)
            ));
        }

        let derived_accounts =
            derive_accounts(&config.seed_phrase, config.network, target_accounts)?;
        initialize_accounts(&state, &derived_accounts).await;

        import_accounts(
            &workspace,
            config.network,
            &seed,
            &account_birthday,
            &transparent_account,
            &derived_accounts[usize::try_from(imported_accounts)
                .map_err(|_| ZeckError::Internal("account index overflowed usize".to_owned()))?
                ..usize::try_from(target_accounts).map_err(|_| {
                    ZeckError::Internal("account index overflowed usize".to_owned())
                })?],
            &state,
        )
        .await?;
        imported_accounts = target_accounts;

        {
            let mut guard = state.lock().await;
            guard.progress.phase = ScanPhase::ScanningShielded;
            guard.progress.message = Some(format!(
                "Syncing compact blocks and transparent UTXOs for {imported_accounts} imported legacy account slots."
            ));
        }

        run_wallet_sync_with_retry(
            &workspace,
            &network,
            &mut client,
            &config.lightwalletd_url,
            &state,
        )
        .await?;
        refresh_scan_progress(&state, &workspace, config.network, effective_birthday).await?;

        if config.num_accounts.is_some() || imported_accounts == max_accounts {
            break;
        }

        let should_stop = {
            let guard = state.lock().await;
            trailing_gap_limit_reached(&guard.progress.accounts, config.gap_limit)
        };
        if should_stop {
            break;
        }

        target_accounts = (target_accounts + config.gap_limit).min(max_accounts);
    }

    let (workspace_dir, total_zatoshis) = {
        let guard = state.lock().await;
        let total = guard
            .progress
            .accounts
            .iter()
            .try_fold(0u64, |sum, account| {
                sum.checked_add(account.total_zatoshis).ok_or_else(|| {
                    ZeckError::Internal("recovery total overflowed the supported range".to_owned())
                })
            })?;
        let workspace_dir = guard
            .workspace
            .as_ref()
            .map(|workspace| workspace.root().display().to_string())
            .unwrap_or_default();
        (workspace_dir, total)
    };

    {
        let mut guard = state.lock().await;
        guard.progress.phase = ScanPhase::Complete;
        guard.progress.summary = Some(ScanSummary {
            total_zatoshis,
            authoritative_balances: true,
            note: if total_zatoshis > 0 {
                "Compact-block recovery finished. Transparent, Sapling, and Orchard balances now come from the persisted wallet workspace and are ready for sweep planning."
                    .to_owned()
            } else {
                "Compact-block recovery finished, but no spendable funds were found in the scanned legacy account range."
                    .to_owned()
            },
            workspace_dir,
        });
        guard.progress.message = Some(
            "Recovery scan finished. Review the authoritative per-account balances and continue to the sweep step when you are ready."
                .to_owned(),
        );
    }

    Ok(())
}

fn resolve_max_account_count(config: &RuntimeScanConfig) -> ZeckResult<u32> {
    match config.num_accounts {
        Some(0) => Err(ZeckError::InvalidConfig(
            "num_accounts must be at least 1".to_owned(),
        )),
        Some(count) if count > MAX_ACCOUNT_SCAN_COUNT => Err(ZeckError::InvalidConfig(format!(
            "num_accounts must not exceed {MAX_ACCOUNT_SCAN_COUNT}"
        ))),
        Some(count) => Ok(count),
        None => Ok(MAX_ACCOUNT_SCAN_COUNT),
    }
}

fn initial_batch_size(config: &RuntimeScanConfig, max_accounts: u32) -> u32 {
    config
        .num_accounts
        .unwrap_or(config.gap_limit.min(max_accounts))
}

async fn initialize_accounts(state: &SharedScanTaskState, accounts: &[DerivedAccount]) {
    let mut guard = state.lock().await;
    guard.progress.accounts = accounts.iter().map(build_account_preview).collect();
}

fn build_account_preview(account: &DerivedAccount) -> AccountBalancePreview {
    AccountBalancePreview {
        account_index: account.index,
        sapling_address: account.sapling_address.clone(),
        unified_address: account.unified_address.clone(),
        transparent_receive_address: account.transparent_receive_address.clone(),
        transparent_change_address: account.transparent_change_address.clone(),
        transparent_utxo_count: 0,
        sapling_zatoshis: 0,
        orchard_zatoshis: 0,
        transparent_zatoshis: 0,
        total_zatoshis: 0,
        has_activity: false,
        status: "Derived locally. Waiting for wallet workspace sync.".to_owned(),
    }
}

async fn import_accounts(
    workspace: &RecoveryWorkspace,
    network: crate::models::ZeckNetwork,
    seed: &[u8; 64],
    birthday: &AccountBirthday,
    transparent_account: &zcash_transparent::keys::AccountPrivKey,
    accounts: &[DerivedAccount],
    state: &SharedScanTaskState,
) -> ZeckResult<()> {
    if accounts.is_empty() {
        return Ok(());
    }

    let seed_fingerprint = SeedFingerprint::from_seed(seed).ok_or_else(|| {
        ZeckError::Internal("mnemonic seed length is out of the ZIP 32 range".to_owned())
    })?;
    let mut wallet_db = WalletDb::for_path(
        workspace.wallet_db_path(),
        consensus_network(network),
        SystemClock,
        OsRng,
    )
    .map_err(|err| {
        ZeckError::Storage(format!(
            "opening wallet database {}: {err}",
            workspace.wallet_db_path().display()
        ))
    })?;

    let mut tracked_accounts = Vec::with_capacity(accounts.len());

    for account in accounts {
        let zip32_index = AccountId::try_from(account.index).map_err(|_| {
            ZeckError::InvalidConfig(format!("account index {} is out of range", account.index))
        })?;
        let derivation = Zip32Derivation::new(seed_fingerprint, zip32_index);
        let wallet_account_id = if let Some(existing) =
            wallet_db.get_derived_account(&derivation).map_err(|err| {
                ZeckError::Wallet(format!("loading derived account {}: {err}", account.index))
            })? {
            existing.id()
        } else {
            wallet_db
                .import_account_hd(
                    &format!("zwl_account_{}", account.index),
                    &SecretVec::new(seed.to_vec()),
                    zip32_index,
                    birthday,
                    Some("ZECK ZecWallet Lite recovery"),
                )
                .map_err(|err| {
                    ZeckError::Wallet(format!("importing account {}: {err}", account.index))
                })?
                .0
                .id()
        };

        let external_pubkey =
            legacy_transparent_pubkey(transparent_account, AddressScope::External, account.index)?;
        let internal_pubkey =
            legacy_transparent_pubkey(transparent_account, AddressScope::Internal, account.index)?;
        let external_address = TransparentAddress::from_pubkey(&external_pubkey);
        let internal_address = TransparentAddress::from_pubkey(&internal_pubkey);
        let existing_receivers = wallet_db
            .get_transparent_receivers(wallet_account_id, true, true)
            .map_err(|err| {
                ZeckError::Wallet(format!(
                    "loading transparent receivers for account {}: {err}",
                    account.index
                ))
            })?;

        if !existing_receivers.contains_key(&external_address) {
            wallet_db
                .import_standalone_transparent_pubkey(wallet_account_id, external_pubkey)
                .map_err(|err| {
                    ZeckError::Wallet(format!(
                        "importing external transparent receiver for account {}: {err}",
                        account.index
                    ))
                })?;
        }
        if !existing_receivers.contains_key(&internal_address) {
            wallet_db
                .import_standalone_transparent_pubkey(wallet_account_id, internal_pubkey)
                .map_err(|err| {
                    ZeckError::Wallet(format!(
                        "importing internal transparent receiver for account {}: {err}",
                        account.index
                    ))
                })?;
        }

        tracked_accounts.push(TrackedAccount {
            wallet_account_id,
            derived: account.clone(),
            transparent_receivers: vec![external_address, internal_address],
        });
    }

    let mut guard = state.lock().await;
    guard.tracked_accounts.extend(tracked_accounts);
    Ok(())
}

const MAX_SYNC_RETRIES: u32 = 10;
const SYNC_RETRY_DELAY_SECS: u64 = 5;

/// Runs `run_wallet_sync`, reconnecting to lightwalletd on transient transport
/// errors.  Each reconnection attempt tries all configured endpoints in order.
/// Permanent errors (wallet database corruption, etc.) are returned immediately.
async fn run_wallet_sync_with_retry(
    workspace: &RecoveryWorkspace,
    network: &zcash_protocol::consensus::Network,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    lightwalletd_url: &str,
    state: &SharedScanTaskState,
) -> ZeckResult<()> {
    let mut attempts = 0u32;
    loop {
        match run_wallet_sync(workspace, network, client).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                let msg = err.to_string();
                let is_transport = msg.contains("transport error")
                    || msg.contains("h2 protocol error")
                    || msg.contains("GoAway")
                    || msg.contains("TimedOut")
                    || msg.contains("close_notify")
                    || msg.contains("UnexpectedEof");

                if !is_transport || attempts >= MAX_SYNC_RETRIES {
                    return Err(err);
                }

                attempts += 1;
                warn!(
                    "lightwalletd connection dropped (attempt {attempts}/{MAX_SYNC_RETRIES}), reconnecting in {SYNC_RETRY_DELAY_SECS}s: {msg}"
                );

                {
                    let mut guard = state.lock().await;
                    guard.progress.message = Some(format!(
                        "Connection dropped — reconnecting (attempt {attempts}/{MAX_SYNC_RETRIES})…"
                    ));
                }

                tokio::time::sleep(std::time::Duration::from_secs(SYNC_RETRY_DELAY_SECS)).await;

                match probe_lightwalletd_endpoints(lightwalletd_url).await {
                    Ok((new_client, endpoint, _)) => {
                        *client = new_client;
                        let mut guard = state.lock().await;
                        guard.progress.message = Some(format!(
                            "Reconnected to {endpoint}, resuming sync…"
                        ));
                        guard.progress.server = Some(crate::lightwalletd::build_probe(
                            endpoint,
                            &Default::default(),
                        ));
                    }
                    Err(reconnect_err) => {
                        warn!("reconnect failed: {reconnect_err}");
                        // try again next iteration
                    }
                }
            }
        }
    }
}

pub(crate) async fn run_wallet_sync<ChT>(
    workspace: &RecoveryWorkspace,
    network: &zcash_protocol::consensus::Network,
    client: &mut CompactTxStreamerClient<ChT>,
) -> ZeckResult<()>
where
    ChT: GrpcService<TonicBody>,
    ChT::Error: Into<StdError>,
    ChT::ResponseBody: Body<Data = Bytes> + Send + 'static,
    <ChT::ResponseBody as Body>::Error: Into<StdError> + Send,
{
    let cache_db = SqliteBlockCache::for_path(workspace.cache_db_path()).map_err(|err| {
        ZeckError::Storage(format!(
            "opening cache database {}: {err}",
            workspace.cache_db_path().display()
        ))
    })?;
    let mut wallet_db =
        WalletDb::for_path(workspace.wallet_db_path(), *network, SystemClock, OsRng).map_err(
            |err| {
                ZeckError::Storage(format!(
                    "opening wallet database {}: {err}",
                    workspace.wallet_db_path().display()
                ))
            },
        )?;

    sync::run(client, network, &cache_db, &mut wallet_db, SYNC_BATCH_SIZE)
        .await
        .map_err(|err| ZeckError::Wallet(format!("synchronizing wallet workspace: {err}")))?;

    Ok(())
}

pub(crate) async fn refresh_scan_progress(
    state: &SharedScanTaskState,
    workspace: &RecoveryWorkspace,
    network: crate::models::ZeckNetwork,
    effective_birthday: u32,
) -> ZeckResult<()> {
    let tracked_accounts = {
        let guard = state.lock().await;
        guard.tracked_accounts.clone()
    };

    let wallet_db = WalletDb::for_path(
        workspace.wallet_db_path(),
        consensus_network(network),
        SystemClock,
        OsRng,
    )
    .map_err(|err| {
        ZeckError::Storage(format!(
            "opening wallet database {}: {err}",
            workspace.wallet_db_path().display()
        ))
    })?;

    let summary = wallet_db
        .get_wallet_summary(ConfirmationsPolicy::MIN)
        .map_err(|err| ZeckError::Wallet(format!("loading wallet summary: {err}")))?
        .ok_or_else(|| ZeckError::Wallet("wallet summary is unavailable after sync".to_owned()))?;

    // Open a read-only connection to check historical note activity (including
    // spent notes) per account.  The WalletRead API only exposes current
    // balances, so accounts that received and fully spent funds would appear
    // inactive.  Querying the raw sqlite tables lets us detect any note that was
    // ever received, which is the correct signal for gap-limit decisions.
    let raw_conn = Connection::open_with_flags(
        workspace.wallet_db_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|err| {
        ZeckError::Storage(format!(
            "opening wallet database for activity check: {err}"
        ))
    })?;

    let target_height = (summary.chain_tip_height() + 1).into();
    let mut account_rows = Vec::with_capacity(tracked_accounts.len());
    let mut total_zatoshis = 0u64;

    for tracked in tracked_accounts {
        let balance = summary.account_balances().get(&tracked.wallet_account_id);
        let sapling_zatoshis = balance
            .map(|value| u64::from(value.sapling_balance().total()))
            .unwrap_or(0);
        let orchard_zatoshis = balance
            .map(|value| u64::from(value.orchard_balance().total()))
            .unwrap_or(0);
        let transparent_zatoshis = balance
            .map(|value| u64::from(value.unshielded_balance().total()))
            .unwrap_or(0);
        let total_account_zatoshis = balance.map(|value| u64::from(value.total())).unwrap_or(0);
        total_zatoshis = total_zatoshis
            .checked_add(total_account_zatoshis)
            .ok_or_else(|| {
                ZeckError::Internal("recovery total overflowed the supported range".to_owned())
            })?;

        let transparent_utxo_count =
            tracked
                .transparent_receivers
                .iter()
                .try_fold(0usize, |sum, address| {
                    let outputs = wallet_db
                        .get_spendable_transparent_outputs(
                            address,
                            target_height,
                            ConfirmationsPolicy::MIN,
                        )
                        .map_err(|err| {
                            ZeckError::Wallet(format!(
                                "loading transparent outputs for account {}: {err}",
                                tracked.derived.index
                            ))
                        })?;
                    sum.checked_add(outputs.len()).ok_or_else(|| {
                        ZeckError::Internal(
                            "transparent UTXO count overflowed the supported range".to_owned(),
                        )
                    })
                })?;

        let has_activity = account_has_note_activity(
            &raw_conn,
            &tracked.wallet_account_id,
        )
        .map_err(|err| {
            ZeckError::Wallet(format!(
                "checking note activity for account {}: {err}",
                tracked.derived.index
            ))
        })?;

        account_rows.push(AccountBalancePreview {
            account_index: tracked.derived.index,
            sapling_address: tracked.derived.sapling_address.clone(),
            unified_address: tracked.derived.unified_address.clone(),
            transparent_receive_address: tracked.derived.transparent_receive_address.clone(),
            transparent_change_address: tracked.derived.transparent_change_address.clone(),
            transparent_utxo_count: u32::try_from(transparent_utxo_count).map_err(|_| {
                ZeckError::Internal("transparent UTXO count overflowed u32".to_owned())
            })?,
            sapling_zatoshis,
            orchard_zatoshis,
            transparent_zatoshis,
            total_zatoshis: total_account_zatoshis,
            has_activity,
            status: build_account_status(
                sapling_zatoshis,
                orchard_zatoshis,
                transparent_zatoshis,
                transparent_utxo_count,
                has_activity,
            ),
        });
    }

    let mut guard = state.lock().await;
    let scanned_height = u64::from(u32::from(summary.fully_scanned_height()));
    {
        // Split disjoint borrows on the progress struct so we can compare
        // the prior account snapshot against the new one while extending
        // the discovery log in place.
        let progress = &mut guard.progress;
        append_new_discoveries(
            &mut progress.discoveries,
            &progress.accounts,
            &account_rows,
            scanned_height,
        );
    }
    guard.progress.accounts = account_rows;
    guard.progress.blocks_total =
        block_delta(summary.chain_tip_height().into(), effective_birthday);
    guard.progress.blocks_scanned =
        block_delta(summary.fully_scanned_height().into(), effective_birthday);
    guard.progress.summary = Some(ScanSummary {
        total_zatoshis,
        authoritative_balances: true,
        note: format!(
            "Wallet workspace synced through height {} and is tracking {} imported legacy account slots.",
            u32::from(summary.fully_scanned_height()),
            guard.progress.accounts.len()
        ),
        workspace_dir: workspace.root().display().to_string(),
    });
    guard.progress.message = Some(format!(
        "Wallet workspace synced through height {}. Review the account table below for authoritative balances.",
        u32::from(summary.fully_scanned_height())
    ));

    Ok(())
}

/// Fast transparent-balance probe issued before the shielded compact-block
/// scan begins. Batches every receive + change address from the initial gap
/// window into a single `GetAddressUtxos` call to lightwalletd, then
/// surfaces non-zero balances as preliminary discoveries.
///
/// Side effects on the shared state:
/// - Sets `phase = ScanningTransparent` while the probe is in flight.
/// - Updates `progress.accounts[i].transparent_zatoshis` and
///   `transparent_utxo_count` so that the subsequent shielded sync's
///   refresh tick sees these UTXOs as "previously known" and does not
///   re-emit them as duplicate discoveries.
/// - Pushes one `ScanDiscovery::Transparent` per account that had any
///   transparent funds, with `at_block_height = chain_tip_height`.
async fn run_transparent_quick_probe(
    state: &SharedScanTaskState,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    accounts: &[DerivedAccount],
    chain_tip_height: u32,
) -> ZeckResult<()> {
    use std::collections::HashMap;

    if accounts.is_empty() {
        return Ok(());
    }

    {
        let mut guard = state.lock().await;
        guard.progress.phase = ScanPhase::ScanningTransparent;
        guard.progress.message = Some(format!(
            "Quick-checking transparent balances for {} accounts via lightwalletd…",
            accounts.len()
        ));
    }

    // Build the address batch — receive + change for every account in the
    // initial window. Track account ownership so we can fold UTXO results
    // back into per-account preliminary balances.
    let mut address_to_account: HashMap<String, u32> = HashMap::new();
    let mut addresses: Vec<String> = Vec::with_capacity(accounts.len() * 2);
    for account in accounts {
        for addr in [
            &account.transparent_receive_address,
            &account.transparent_change_address,
        ] {
            if !addr.is_empty() && !address_to_account.contains_key(addr) {
                address_to_account.insert(addr.clone(), account.index);
                addresses.push(addr.clone());
            }
        }
    }
    if addresses.is_empty() {
        return Ok(());
    }

    let reply = client
        .get_address_utxos(GetAddressUtxosArg {
            addresses,
            start_height: 0,
            max_entries: 0,
        })
        .await
        .map_err(|err| ZeckError::Lightwalletd(err.to_string()))?
        .into_inner();

    // Aggregate UTXO value per account.
    let mut sums: HashMap<u32, (u64, u32)> = HashMap::new();
    for utxo in &reply.address_utxos {
        let Some(&account_index) = address_to_account.get(&utxo.address) else {
            continue;
        };
        let entry = sums.entry(account_index).or_insert((0u64, 0u32));
        let value = u64::try_from(utxo.value_zat).unwrap_or(0);
        entry.0 = entry.0.saturating_add(value);
        entry.1 = entry.1.saturating_add(1);
    }

    if sums.is_empty() {
        return Ok(());
    }

    let mut guard = state.lock().await;
    let chain_tip = u64::from(chain_tip_height);
    for account in &mut guard.progress.accounts {
        if let Some(&(zatoshis, utxo_count)) = sums.get(&account.account_index) {
            if zatoshis == 0 {
                continue;
            }
            account.transparent_zatoshis = zatoshis;
            account.transparent_utxo_count = utxo_count;
            account.total_zatoshis = account
                .sapling_zatoshis
                .saturating_add(account.orchard_zatoshis)
                .saturating_add(zatoshis);
            account.has_activity = true;
            account.status = format!(
                "Preliminary: {utxo_count} transparent UTXOs / {zatoshis} zats (shielded scan still pending)."
            );
        }
    }
    for (account_index, (zatoshis, _)) in sums {
        if zatoshis == 0 {
            continue;
        }
        let address = guard
            .progress
            .accounts
            .iter()
            .find(|a| a.account_index == account_index)
            .map(|a| a.transparent_receive_address.clone())
            .unwrap_or_default();
        guard.progress.discoveries.push(ScanDiscovery {
            account_index,
            pool: DiscoveryPool::Transparent,
            zatoshis,
            at_block_height: chain_tip,
            address,
        });
    }
    guard.progress.message = Some(
        "Transparent quick-check complete. Continuing with shielded compact-block scan…"
            .to_owned(),
    );

    Ok(())
}

/// Walk the new account snapshot, append a `ScanDiscovery` to `discoveries`
/// for every (account, pool) pair that newly has a non-zero balance compared
/// to the previous snapshot. Append-only: discoveries already in the log are
/// never modified or removed, even if the corresponding balance later falls
/// to zero (so users can see "yes, this seed had funds" even if the wallet
/// was already swept).
fn append_new_discoveries(
    discoveries: &mut Vec<crate::models::ScanDiscovery>,
    previous: &[AccountBalancePreview],
    current: &[AccountBalancePreview],
    at_block_height: u64,
) {
    use crate::models::{DiscoveryPool, ScanDiscovery};

    let prev_by_index: std::collections::HashMap<u32, &AccountBalancePreview> =
        previous.iter().map(|a| (a.account_index, a)).collect();

    for account in current {
        let was_zero = |amount: u64, accessor: fn(&AccountBalancePreview) -> u64| {
            if amount == 0 {
                return false;
            }
            match prev_by_index.get(&account.account_index) {
                Some(prev) => accessor(prev) == 0,
                None => true,
            }
        };

        if was_zero(account.transparent_zatoshis, |a| a.transparent_zatoshis) {
            discoveries.push(ScanDiscovery {
                account_index: account.account_index,
                pool: DiscoveryPool::Transparent,
                zatoshis: account.transparent_zatoshis,
                at_block_height,
                address: account.transparent_receive_address.clone(),
            });
        }
        if was_zero(account.sapling_zatoshis, |a| a.sapling_zatoshis) {
            discoveries.push(ScanDiscovery {
                account_index: account.account_index,
                pool: DiscoveryPool::Sapling,
                zatoshis: account.sapling_zatoshis,
                at_block_height,
                address: account.sapling_address.clone(),
            });
        }
        if was_zero(account.orchard_zatoshis, |a| a.orchard_zatoshis) {
            discoveries.push(ScanDiscovery {
                account_index: account.account_index,
                pool: DiscoveryPool::Orchard,
                zatoshis: account.orchard_zatoshis,
                at_block_height,
                address: account.unified_address.clone(),
            });
        }
    }
}

fn build_account_status(
    sapling_zatoshis: u64,
    orchard_zatoshis: u64,
    transparent_zatoshis: u64,
    transparent_utxo_count: usize,
    has_activity: bool,
) -> String {
    let total = sapling_zatoshis + orchard_zatoshis + transparent_zatoshis;
    if total == 0 {
        return if has_activity {
            "Previously active (all funds spent).".to_owned()
        } else {
            "No funds found for this derived account.".to_owned()
        };
    }

    let mut parts = Vec::new();
    if transparent_zatoshis > 0 {
        parts.push(format!(
            "{transparent_utxo_count} transparent UTXOs / {transparent_zatoshis} zats"
        ));
    }
    if sapling_zatoshis > 0 {
        parts.push(format!("Sapling {sapling_zatoshis} zats"));
    }
    if orchard_zatoshis > 0 {
        parts.push(format!("Orchard {orchard_zatoshis} zats"));
    }

    format!("Found {}.", parts.join(", "))
}

fn trailing_gap_limit_reached(accounts: &[AccountBalancePreview], gap_limit: u32) -> bool {
    let gap = usize::try_from(gap_limit).unwrap_or(usize::MAX);
    if accounts.len() < gap {
        return false;
    }

    accounts
        .iter()
        .rev()
        .take(gap)
        .all(|account| !account.has_activity)
}

/// Returns `true` if the wallet database contains any received notes (Sapling,
/// Orchard, or transparent) for the given account, regardless of whether those
/// notes have been spent.  This is the correct activity signal for gap-limit
/// decisions: an account that received and fully spent its funds is still
/// evidence that higher account indices may also be in use.
fn account_has_note_activity(
    conn: &Connection,
    account_uuid: &AccountUuid,
) -> Result<bool, rusqlite::Error> {
    let uuid_bytes = account_uuid.expose_uuid().into_bytes();
    // Resolve the internal integer id once to avoid repeating the subquery and
    // to sidestep potential issues if uuid is not unique-constrained.
    let account_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM accounts WHERE uuid = ?1",
            params![uuid_bytes.as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let account_id = match account_id {
        Some(id) => id,
        None => return Ok(false),
    };
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sapling_received_notes WHERE account_id = ?1)
             OR EXISTS(SELECT 1 FROM orchard_received_notes WHERE account_id = ?1)
             OR EXISTS(SELECT 1 FROM transparent_received_outputs WHERE account_id = ?1)",
        params![account_id],
        |row| row.get(0),
    )
}

fn block_delta(height: u32, birthday: u32) -> u64 {
    u64::from(height.saturating_sub(birthday).saturating_add(1))
}

async fn check_cancelled(state: &SharedScanTaskState) -> ZeckResult<()> {
    let cancelled = {
        let guard = state.lock().await;
        guard.cancelled.load(Ordering::SeqCst)
    };

    if cancelled {
        let mut guard = state.lock().await;
        guard.progress.phase = ScanPhase::Cancelled;
        guard.progress.message = Some("Recovery scan cancelled.".to_owned());
        warn!("scan {} cancelled", guard.progress.handle.id);
        return Err(ZeckError::Cancelled);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use super::{
        append_new_discoveries, build_account_status, resolve_max_account_count,
        trailing_gap_limit_reached,
    };
    use crate::models::{
        AccountBalancePreview, DiscoveryPool, RuntimeScanConfig, ScanDiscovery, ZeckNetwork,
    };

    fn config(num_accounts: Option<u32>) -> RuntimeScanConfig {
        RuntimeScanConfig {
            seed_phrase: SecretString::new(
                "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"
                    .to_owned(),
            ),
            birthday: 419_200,
            num_accounts,
            gap_limit: 20,
            lightwalletd_url: "https://example.com".to_owned(),
            data_dir: std::path::PathBuf::from("zeck_data"),
            network: ZeckNetwork::Mainnet,
        }
    }

    #[test]
    fn account_limit_defaults_to_ceiling_for_gap_limit_mode() {
        let count = resolve_max_account_count(&config(None)).expect("default account count");
        assert_eq!(count, 500);
    }

    #[test]
    fn account_limit_rejects_zero() {
        let err = resolve_max_account_count(&config(Some(0))).expect_err("zero should fail");
        assert!(err.to_string().contains("at least 1"));
    }

    #[test]
    fn account_status_mentions_shielded_and_transparent_funds() {
        let status = build_account_status(42_000, 84_000, 21_000, 2, true);
        assert!(status.contains("Sapling"));
        assert!(status.contains("Orchard"));
        assert!(status.contains("transparent"));
    }

    #[test]
    fn account_status_shows_previously_active_for_spent_account() {
        let status = build_account_status(0, 0, 0, 0, true);
        assert!(status.contains("Previously active"));
    }

    #[test]
    fn account_status_shows_no_funds_for_inactive_account() {
        let status = build_account_status(0, 0, 0, 0, false);
        assert!(status.contains("No funds found"));
    }

    #[test]
    fn gap_limit_only_triggers_on_trailing_inactive_accounts() {
        let accounts = vec![
            AccountBalancePreview {
                account_index: 0,
                sapling_address: "zs".to_owned(),
                unified_address: "u".to_owned(),
                transparent_receive_address: "t1".to_owned(),
                transparent_change_address: "t2".to_owned(),
                transparent_utxo_count: 0,
                sapling_zatoshis: 1,
                orchard_zatoshis: 0,
                transparent_zatoshis: 0,
                total_zatoshis: 1,
                has_activity: true,
                status: "found".to_owned(),
            },
            AccountBalancePreview {
                account_index: 1,
                sapling_address: "zs".to_owned(),
                unified_address: "u".to_owned(),
                transparent_receive_address: "t1".to_owned(),
                transparent_change_address: "t2".to_owned(),
                transparent_utxo_count: 0,
                sapling_zatoshis: 0,
                orchard_zatoshis: 0,
                transparent_zatoshis: 0,
                total_zatoshis: 0,
                has_activity: false,
                status: "empty".to_owned(),
            },
            AccountBalancePreview {
                account_index: 2,
                sapling_address: "zs".to_owned(),
                unified_address: "u".to_owned(),
                transparent_receive_address: "t1".to_owned(),
                transparent_change_address: "t2".to_owned(),
                transparent_utxo_count: 0,
                sapling_zatoshis: 0,
                orchard_zatoshis: 0,
                transparent_zatoshis: 0,
                total_zatoshis: 0,
                has_activity: false,
                status: "empty".to_owned(),
            },
        ];

        assert!(trailing_gap_limit_reached(&accounts, 2));
        assert!(!trailing_gap_limit_reached(&accounts, 3));
    }

    #[test]
    fn gap_limit_does_not_trigger_when_spent_account_in_trailing_window() {
        // Account 1 has zero balance but historical activity (received and spent).
        // The gap limit should NOT trigger because account 1 is still "active".
        let accounts = vec![
            AccountBalancePreview {
                account_index: 0,
                sapling_address: "zs".to_owned(),
                unified_address: "u".to_owned(),
                transparent_receive_address: "t1".to_owned(),
                transparent_change_address: "t2".to_owned(),
                transparent_utxo_count: 0,
                sapling_zatoshis: 1,
                orchard_zatoshis: 0,
                transparent_zatoshis: 0,
                total_zatoshis: 1,
                has_activity: true,
                status: "found".to_owned(),
            },
            AccountBalancePreview {
                account_index: 1,
                sapling_address: "zs".to_owned(),
                unified_address: "u".to_owned(),
                transparent_receive_address: "t1".to_owned(),
                transparent_change_address: "t2".to_owned(),
                transparent_utxo_count: 0,
                sapling_zatoshis: 0,
                orchard_zatoshis: 0,
                transparent_zatoshis: 0,
                total_zatoshis: 0,
                has_activity: true, // spent account -- still active
                status: "previously active".to_owned(),
            },
            AccountBalancePreview {
                account_index: 2,
                sapling_address: "zs".to_owned(),
                unified_address: "u".to_owned(),
                transparent_receive_address: "t1".to_owned(),
                transparent_change_address: "t2".to_owned(),
                transparent_utxo_count: 0,
                sapling_zatoshis: 0,
                orchard_zatoshis: 0,
                transparent_zatoshis: 0,
                total_zatoshis: 0,
                has_activity: false,
                status: "empty".to_owned(),
            },
        ];

        // With gap_limit=2, the trailing 2 accounts are [1, 2].
        // Account 1 has_activity=true, so the gap limit should NOT fire.
        assert!(!trailing_gap_limit_reached(&accounts, 2));
    }

    #[test]
    fn gap_limit_boundary_with_spent_account_at_edge() {
        // Layout: [active, empty, spent] with gap_limit=2.
        // Trailing window = [empty, spent]. The spent account has activity,
        // so the gap limit should NOT fire.
        let accounts = vec![
            AccountBalancePreview {
                account_index: 0,
                sapling_address: "zs".to_owned(),
                unified_address: "u".to_owned(),
                transparent_receive_address: "t1".to_owned(),
                transparent_change_address: "t2".to_owned(),
                transparent_utxo_count: 0,
                sapling_zatoshis: 100,
                orchard_zatoshis: 0,
                transparent_zatoshis: 0,
                total_zatoshis: 100,
                has_activity: true,
                status: "found".to_owned(),
            },
            AccountBalancePreview {
                account_index: 1,
                sapling_address: "zs".to_owned(),
                unified_address: "u".to_owned(),
                transparent_receive_address: "t1".to_owned(),
                transparent_change_address: "t2".to_owned(),
                transparent_utxo_count: 0,
                sapling_zatoshis: 0,
                orchard_zatoshis: 0,
                transparent_zatoshis: 0,
                total_zatoshis: 0,
                has_activity: false,
                status: "empty".to_owned(),
            },
            AccountBalancePreview {
                account_index: 2,
                sapling_address: "zs".to_owned(),
                unified_address: "u".to_owned(),
                transparent_receive_address: "t1".to_owned(),
                transparent_change_address: "t2".to_owned(),
                transparent_utxo_count: 0,
                sapling_zatoshis: 0,
                orchard_zatoshis: 0,
                transparent_zatoshis: 0,
                total_zatoshis: 0,
                has_activity: true, // spent account at boundary
                status: "previously active".to_owned(),
            },
        ];

        // Trailing 2 = [empty, spent]. Spent has activity, so gap limit does NOT fire.
        assert!(!trailing_gap_limit_reached(&accounts, 2));
        // But with gap_limit=1, trailing 1 = [spent], which has activity -- still no trigger.
        assert!(!trailing_gap_limit_reached(&accounts, 1));
    }

    fn empty_account(index: u32) -> AccountBalancePreview {
        AccountBalancePreview {
            account_index: index,
            sapling_address: "zs".to_owned(),
            unified_address: "u".to_owned(),
            transparent_receive_address: "t1".to_owned(),
            transparent_change_address: "t2".to_owned(),
            transparent_utxo_count: 0,
            sapling_zatoshis: 0,
            orchard_zatoshis: 0,
            transparent_zatoshis: 0,
            total_zatoshis: 0,
            has_activity: false,
            status: "empty".to_owned(),
        }
    }

    fn active_account(index: u32) -> AccountBalancePreview {
        AccountBalancePreview {
            account_index: index,
            sapling_zatoshis: 1,
            total_zatoshis: 1,
            has_activity: true,
            status: "found".to_owned(),
            ..empty_account(index)
        }
    }

    #[test]
    fn gap_limit_1_triggers_on_single_trailing_empty_account() {
        // [active, empty] with gap_limit=1 → trailing window is [empty] → fires
        let accounts = vec![active_account(0), empty_account(1)];
        assert!(trailing_gap_limit_reached(&accounts, 1));
    }

    #[test]
    fn gap_limit_1_does_not_trigger_on_active_tail() {
        // [empty, active] with gap_limit=1 → trailing window is [active] → no fire
        let accounts = vec![empty_account(0), active_account(1)];
        assert!(!trailing_gap_limit_reached(&accounts, 1));
    }

    #[test]
    fn gap_limit_triggers_only_when_all_trailing_accounts_inactive() {
        // [active, empty, empty] with gap_limit=2 → both trailing are inactive → fires
        let accounts = vec![active_account(0), empty_account(1), empty_account(2)];
        assert!(trailing_gap_limit_reached(&accounts, 2));
        // with gap_limit=1 → only last is empty → also fires
        assert!(trailing_gap_limit_reached(&accounts, 1));
        // with gap_limit=3 → window covers all 3, first has activity → no fire
        assert!(!trailing_gap_limit_reached(&accounts, 3));
    }

    #[test]
    fn gap_limit_larger_than_account_count_never_triggers() {
        let accounts = vec![empty_account(0), empty_account(1)];
        // gap_limit=5 > 2 accounts → window is entire list; but since there are
        // fewer accounts than the gap_limit, scanning has not yet had enough room
        // to confirm absence — should not fire.
        assert!(!trailing_gap_limit_reached(&accounts, 5));
    }

    fn account_with(
        index: u32,
        sapling: u64,
        orchard: u64,
        transparent: u64,
    ) -> AccountBalancePreview {
        AccountBalancePreview {
            account_index: index,
            sapling_zatoshis: sapling,
            orchard_zatoshis: orchard,
            transparent_zatoshis: transparent,
            total_zatoshis: sapling + orchard + transparent,
            has_activity: sapling + orchard + transparent > 0,
            ..empty_account(index)
        }
    }

    #[test]
    fn first_observation_emits_one_discovery_per_funded_pool() {
        let mut log = Vec::new();
        let new_snapshot = vec![account_with(0, 100, 200, 300)];
        append_new_discoveries(&mut log, &[], &new_snapshot, 3_280_500);
        assert_eq!(log.len(), 3);
        let pools: Vec<DiscoveryPool> = log.iter().map(|d| d.pool).collect();
        assert!(pools.contains(&DiscoveryPool::Transparent));
        assert!(pools.contains(&DiscoveryPool::Sapling));
        assert!(pools.contains(&DiscoveryPool::Orchard));
        for d in &log {
            assert_eq!(d.account_index, 0);
            assert_eq!(d.at_block_height, 3_280_500);
            assert!(d.zatoshis > 0);
        }
    }

    #[test]
    fn empty_accounts_emit_no_discoveries() {
        let mut log = Vec::new();
        let snapshot = vec![empty_account(0), empty_account(1)];
        append_new_discoveries(&mut log, &[], &snapshot, 100);
        assert!(log.is_empty());
    }

    #[test]
    fn unchanged_balance_does_not_re_emit() {
        let mut log = Vec::new();
        let prev = vec![account_with(0, 100, 0, 0)];
        let next = vec![account_with(0, 100, 0, 0)];
        append_new_discoveries(&mut log, &prev, &next, 100);
        assert!(
            log.is_empty(),
            "discovery already in prior snapshot must not be re-emitted"
        );
    }

    #[test]
    fn newly_funded_pool_on_existing_account_emits_one_discovery() {
        // Account 0 gained orchard funds; sapling was already known.
        let mut log = Vec::new();
        let prev = vec![account_with(0, 100, 0, 0)];
        let next = vec![account_with(0, 100, 50, 0)];
        append_new_discoveries(&mut log, &prev, &next, 200);
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].pool, DiscoveryPool::Orchard);
        assert_eq!(log[0].zatoshis, 50);
    }

    #[test]
    fn balance_dropping_to_zero_does_not_remove_existing_discovery() {
        // First tick discovers Sapling 100; second tick shows it spent.
        // The existing discovery must remain (append-only).
        let mut log = vec![ScanDiscovery {
            account_index: 0,
            pool: DiscoveryPool::Sapling,
            zatoshis: 100,
            at_block_height: 50,
            address: "zs".to_owned(),
        }];
        let prev = vec![account_with(0, 100, 0, 0)];
        let next = vec![account_with(0, 0, 0, 0)];
        append_new_discoveries(&mut log, &prev, &next, 75);
        assert_eq!(log.len(), 1, "previous discovery must be preserved");
        assert_eq!(log[0].zatoshis, 100, "stored zatoshis must not be mutated");
    }

    #[test]
    fn newly_appearing_account_emits_for_each_funded_pool() {
        // Gap-limit extension can introduce new accounts mid-scan.
        let mut log = Vec::new();
        let prev = vec![account_with(0, 100, 0, 0)];
        let next = vec![account_with(0, 100, 0, 0), account_with(7, 0, 50, 0)];
        append_new_discoveries(&mut log, &prev, &next, 300);
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].account_index, 7);
        assert_eq!(log[0].pool, DiscoveryPool::Orchard);
    }
}
