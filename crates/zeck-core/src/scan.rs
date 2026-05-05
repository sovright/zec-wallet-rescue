use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

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
        wallet::ConfirmationsPolicy,
        Account as _, AccountBirthday, InputSource, WalletRead, WalletWrite, Zip32Derivation,
    },
    proto::service::{
        compact_tx_streamer_client::CompactTxStreamerClient, BlockId, GetAddressUtxosArg,
    },
    sync,
};
use zcash_client_sqlite::{util::SystemClock, AccountUuid, WalletDb};
use zcash_protocol::consensus::BlockHeight;
use zcash_transparent::address::TransparentAddress;
use zip32::{fingerprint::SeedFingerprint, AccountId};

use crate::{
    cache::SqliteBlockCache,
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
                synced_to_height: None,
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

struct ProgressPoller {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl ProgressPoller {
    fn start(
        workspace: RecoveryWorkspace,
        network: crate::models::ZeckNetwork,
        state: SharedScanTaskState,
        effective_birthday: u32,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let task = tokio::spawn(async move {
            let scan_started = std::time::Instant::now();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(db) = WalletDb::for_path(
                    workspace.wallet_db_path(),
                    consensus_network(network),
                    SystemClock,
                    OsRng,
                ) {
                    if let Ok(Some(summary)) = db.get_wallet_summary(ConfirmationsPolicy::MIN) {
                        let scanned_height = u32::from(summary.fully_scanned_height());
                        let mut guard = state.lock().await;
                        guard.progress.blocks_scanned =
                            block_delta(scanned_height, effective_birthday);
                        guard.progress.synced_to_height = Some(u64::from(scanned_height));
                        // Store scan-phase elapsed so get_scan_progress can compute an
                        // accurate rate that excludes pre-scan phases (seed validation,
                        // key derivation, lightwalletd probing).
                        guard.progress.elapsed_seconds =
                            Some(scan_started.elapsed().as_secs());
                    }
                }
            }
        });
        Self { stop, task }
    }

    async fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.task.await;
    }
}

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

        // Fast transparent-only probe over the newly-added slice for this
        // iteration. lightwalletd's GetAddressUtxos answers in milliseconds
        // and surfaces preliminary t-addr balances long before the shielded
        // sync finishes. Probing per gap-extension iteration (rather than
        // only the first batch) means a funded account that only appears
        // after gap extension still gets the early-discovery UX.
        //
        // Safety: the probe dedupes its discovery pushes against the
        // existing log, and we slice to only the newly-derived range, so
        // repeated calls don't produce duplicate events. Failures are
        // non-fatal — the shielded scan below is authoritative.
        let new_slice_start = usize::try_from(imported_accounts)
            .map_err(|_| ZeckError::Internal("account index overflowed usize".to_owned()))?;
        let new_slice_end = usize::try_from(target_accounts)
            .map_err(|_| ZeckError::Internal("account index overflowed usize".to_owned()))?;
        let new_accounts = &derived_accounts[new_slice_start..new_slice_end];
        if let Err(err) =
            run_transparent_quick_probe(&state, &mut client, new_accounts, chain_tip_height).await
        {
            warn!("transparent quick probe failed (continuing with shielded scan): {err}");
        }

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

        let poller = ProgressPoller::start(
            workspace.clone(),
            config.network,
            state.clone(),
            effective_birthday,
        );
        let sync_result = run_wallet_sync_with_retry(
            &workspace,
            &network,
            &mut client,
            &config.lightwalletd_url,
            &state,
        )
        .await;
        poller.stop().await;
        sync_result?;
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
    append_new_discoveries(
        &mut guard.progress.discoveries,
        &account_rows,
        scanned_height,
    );
    guard.progress.accounts = account_rows;
    guard.progress.blocks_total =
        block_delta(summary.chain_tip_height().into(), effective_birthday);
    guard.progress.blocks_scanned =
        block_delta(summary.fully_scanned_height().into(), effective_birthday);
    guard.progress.synced_to_height =
        Some(u64::from(u32::from(summary.fully_scanned_height())));
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
/// scan begins. Batches every receive + change address from the supplied
/// slice into a single `GetAddressUtxos` call to lightwalletd, then
/// surfaces non-zero balances as preliminary discoveries.
///
/// Safe to call multiple times during a scan (e.g. once per gap-limit
/// extension): every discovery push is deduped against the existing
/// `progress.discoveries` log, so probing an already-probed account is
/// a no-op rather than a duplicate emission. Pass only the new account
/// slice each iteration to avoid wasted gRPC traffic.
///
/// Side effects on the shared state:
/// - Sets `phase = ScanningTransparent` while the probe is in flight.
/// - Updates `progress.accounts[i].transparent_zatoshis` and
///   `transparent_utxo_count` for any matched account so the subsequent
///   shielded refresh observes the same number authoritatively.
/// - Pushes a `ScanDiscovery::Transparent` per *newly-funded* account
///   with `at_block_height = chain_tip_height`.
async fn run_transparent_quick_probe(
    state: &SharedScanTaskState,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    accounts: &[DerivedAccount],
    chain_tip_height: u32,
) -> ZeckResult<()> {
    use std::collections::{HashMap, HashSet};

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
    // supplied slice. Track account ownership so we can fold UTXO results
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

    // Aggregate UTXO value per account. A negative value_zat from
    // lightwalletd is misbehaving-server data — log it loudly and skip
    // the entry rather than silently coercing to 0, which would mask
    // the bug from the user.
    let mut sums: HashMap<u32, (u64, u32)> = HashMap::new();
    for utxo in &reply.address_utxos {
        let Some(&account_index) = address_to_account.get(&utxo.address) else {
            continue;
        };
        let value = match u64::try_from(utxo.value_zat) {
            Ok(v) => v,
            Err(_) => {
                warn!(
                    "lightwalletd returned negative value_zat={} for address {} \
                     (account {}); skipping entry",
                    utxo.value_zat, utxo.address, account_index
                );
                continue;
            }
        };
        let entry = sums.entry(account_index).or_insert((0u64, 0u32));
        entry.0 = entry.0.saturating_add(value);
        entry.1 = entry.1.saturating_add(1);
    }

    if sums.is_empty() {
        return Ok(());
    }

    let mut guard = state.lock().await;
    let chain_tip = u64::from(chain_tip_height);

    // Preliminary balance write into the in-memory snapshot. This
    // intentionally clobbers existing preliminary values — a re-probe
    // on the same account should reflect the latest lightwalletd
    // numbers, not the previous tick's.
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

    // Discovery push deduped against the existing log so safe to call
    // the probe multiple times per scan (gap-extension iterations).
    let already_discovered: HashSet<(u32, DiscoveryPool)> = guard
        .progress
        .discoveries
        .iter()
        .map(|d| (d.account_index, d.pool))
        .collect();
    for (account_index, (zatoshis, _)) in sums {
        if zatoshis == 0 {
            continue;
        }
        if already_discovered.contains(&(account_index, DiscoveryPool::Transparent)) {
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
/// Dedupe `(account, pool)` discoveries against the existing append-only
/// `discoveries` log rather than against the previous account snapshot.
///
/// The previous-snapshot approach was unsound: the gap-limit loop calls
/// `initialize_accounts` between batches, which replaces `progress.accounts`
/// with fresh zero-balance previews. Diffing against that zeroed snapshot
/// causes already-known discoveries to be re-emitted on every gap-extension
/// pass, and likewise causes the transparent quick probe's preliminary
/// values to be re-emitted by the first authoritative refresh.
///
/// The append-only log is the authoritative source of "has this
/// `(account, pool)` been surfaced to the user yet?", so dedupe against it.
fn append_new_discoveries(
    discoveries: &mut Vec<crate::models::ScanDiscovery>,
    current: &[AccountBalancePreview],
    at_block_height: u64,
) {
    use crate::models::{DiscoveryPool, ScanDiscovery};

    let mut seen: std::collections::HashSet<(u32, DiscoveryPool)> = discoveries
        .iter()
        .map(|d| (d.account_index, d.pool))
        .collect();

    let mut try_append =
        |discoveries: &mut Vec<ScanDiscovery>,
         account_index: u32,
         pool: DiscoveryPool,
         zatoshis: u64,
         address: String| {
            if zatoshis == 0 {
                return;
            }
            if !seen.insert((account_index, pool)) {
                return;
            }
            discoveries.push(ScanDiscovery {
                account_index,
                pool,
                zatoshis,
                at_block_height,
                address,
            });
        };

    for account in current {
        try_append(
            discoveries,
            account.account_index,
            DiscoveryPool::Transparent,
            account.transparent_zatoshis,
            account.transparent_receive_address.clone(),
        );
        try_append(
            discoveries,
            account.account_index,
            DiscoveryPool::Sapling,
            account.sapling_zatoshis,
            account.sapling_address.clone(),
        );
        try_append(
            discoveries,
            account.account_index,
            DiscoveryPool::Orchard,
            account.orchard_zatoshis,
            account.unified_address.clone(),
        );
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

/// Imports account-0 into a probe workspace without requiring a `SharedScanTaskState`.
/// Used by `birthday::probe_shielded_window` to set up a fresh temporary workspace
/// before running a time-limited sync to detect shielded activity.
pub(crate) fn import_probe_account(
    workspace: &RecoveryWorkspace,
    network: crate::models::ZeckNetwork,
    seed: &[u8; 64],
    birthday: &AccountBirthday,
    transparent_account: &zcash_transparent::keys::AccountPrivKey,
) -> ZeckResult<()> {
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
            "opening probe wallet database {}: {err}",
            workspace.wallet_db_path().display()
        ))
    })?;

    let zip32_index = AccountId::ZERO;
    let derivation = Zip32Derivation::new(seed_fingerprint, zip32_index);

    if wallet_db
        .get_derived_account(&derivation)
        .map_err(|err| ZeckError::Wallet(format!("checking probe account: {err}")))?
        .is_none()
    {
        wallet_db
            .import_account_hd(
                "probe_account_0",
                &SecretVec::new(seed.to_vec()),
                zip32_index,
                birthday,
                None,
            )
            .map_err(|err| ZeckError::Wallet(format!("importing probe account: {err}")))?;
    }

    let wallet_account_id = wallet_db
        .get_derived_account(&derivation)
        .map_err(|err| ZeckError::Wallet(format!("loading probe account after import: {err}")))?
        .ok_or_else(|| ZeckError::Wallet("probe account missing after import".to_owned()))?
        .id();

    let external_pubkey =
        legacy_transparent_pubkey(transparent_account, AddressScope::External, 0)?;
    let existing_receivers = wallet_db
        .get_transparent_receivers(wallet_account_id, true, true)
        .map_err(|err| {
            ZeckError::Wallet(format!("loading probe transparent receivers: {err}"))
        })?;
    let external_address = TransparentAddress::from_pubkey(&external_pubkey);

    if !existing_receivers.contains_key(&external_address) {
        wallet_db
            .import_standalone_transparent_pubkey(wallet_account_id, external_pubkey)
            .map_err(|err| {
                ZeckError::Wallet(format!("importing probe transparent receiver: {err}"))
            })?;
    }

    Ok(())
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
        append_new_discoveries(&mut log, &new_snapshot, 3_280_500);
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
        append_new_discoveries(&mut log, &snapshot, 100);
        assert!(log.is_empty());
    }

    #[test]
    fn second_call_with_same_funded_account_does_not_re_emit() {
        // First call discovers sapling. Second call (e.g. another refresh
        // tick) must not re-emit the same (account, pool) discovery.
        let mut log = Vec::new();
        let snapshot = vec![account_with(0, 100, 0, 0)];
        append_new_discoveries(&mut log, &snapshot, 100);
        assert_eq!(log.len(), 1);
        append_new_discoveries(&mut log, &snapshot, 200);
        assert_eq!(log.len(), 1, "duplicate discovery must not be appended");
    }

    #[test]
    fn newly_funded_pool_on_existing_account_emits_one_discovery() {
        // Account 0 already has sapling discovered; second call shows
        // orchard funds appearing on the same account.
        let mut log = Vec::new();
        let first = vec![account_with(0, 100, 0, 0)];
        let second = vec![account_with(0, 100, 50, 0)];
        append_new_discoveries(&mut log, &first, 100);
        append_new_discoveries(&mut log, &second, 200);
        assert_eq!(log.len(), 2);
        assert_eq!(log[1].pool, DiscoveryPool::Orchard);
        assert_eq!(log[1].zatoshis, 50);
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
        let next = vec![account_with(0, 0, 0, 0)];
        append_new_discoveries(&mut log, &next, 75);
        assert_eq!(log.len(), 1, "previous discovery must be preserved");
        assert_eq!(log[0].zatoshis, 100, "stored zatoshis must not be mutated");
    }

    #[test]
    fn newly_appearing_account_emits_for_each_funded_pool() {
        // Gap-limit extension can introduce new accounts between calls.
        let mut log = Vec::new();
        let first = vec![account_with(0, 100, 0, 0)];
        let second = vec![account_with(0, 100, 0, 0), account_with(7, 0, 50, 0)];
        append_new_discoveries(&mut log, &first, 100);
        append_new_discoveries(&mut log, &second, 200);
        assert_eq!(log.len(), 2);
        assert_eq!(log[1].account_index, 7);
        assert_eq!(log[1].pool, DiscoveryPool::Orchard);
    }

    #[test]
    fn initialize_accounts_zeroing_does_not_cause_duplicate_emission() {
        // Regression test for the gap-limit-extension bug. The real scan
        // loop calls initialize_accounts() between batches, which zeros
        // the in-memory snapshot. The dedup logic must not re-emit the
        // same (account, pool) just because the snapshot was wiped and
        // refilled.
        //
        // Scenario:
        //   1. Authoritative refresh observes account 0 with 500 sapling.
        //   2. Loop extends gap range; initialize_accounts wipes snapshot
        //      to zeros (this is what previous logic compared against).
        //   3. Next refresh observes account 0 still with 500 sapling
        //      (it didn't disappear from WalletDb).
        // Expected: only one Sapling discovery for account 0 in the log.
        let mut log = Vec::new();
        let funded = vec![account_with(0, 500, 0, 0)];
        append_new_discoveries(&mut log, &funded, 100);
        assert_eq!(log.len(), 1);
        // Step 2: snapshot was zeroed by initialize_accounts. Step 3:
        // refresh sees the same funded account again. Old logic would
        // see prev=0, current=500, and re-emit. New logic dedupes
        // against the existing discovery log.
        append_new_discoveries(&mut log, &funded, 200);
        assert_eq!(
            log.len(),
            1,
            "gap-limit extension must not produce duplicate discoveries"
        );
    }

    #[test]
    fn transparent_quick_probe_followed_by_authoritative_refresh_dedupes() {
        // Regression test for PR #13's invariant. The transparent quick
        // probe pushes ScanDiscovery::Transparent directly. The first
        // authoritative refresh then calls append_new_discoveries with
        // a snapshot that may or may not have transparent_zatoshis set.
        // Either way, the existing discovery in the log must dedupe it.
        let mut log = vec![ScanDiscovery {
            account_index: 0,
            pool: DiscoveryPool::Transparent,
            zatoshis: 500_000,
            at_block_height: 3_280_500,
            address: "t1".to_owned(),
        }];
        // Refresh sees the same balance authoritatively; must not duplicate.
        let snapshot = vec![account_with(0, 0, 0, 500_000)];
        append_new_discoveries(&mut log, &snapshot, 3_281_000);
        assert_eq!(
            log.len(),
            1,
            "authoritative refresh must not re-emit a probe discovery"
        );
    }

    /// Cancel-then-resume workspace persistence tests.
    ///
    /// These tests exercise the invariant that:
    ///   1. `import_accounts` leaves a persistent SQLite wallet DB on disk
    ///      (matching what happens when a scan task is `abort()`-ed mid-flight).
    ///   2. A second scan started with the same `RuntimeScanConfig` resolves to
    ///      the same workspace directory and does not duplicate already-imported
    ///      accounts.
    ///
    /// A mock lightwalletd gRPC server is not required because these properties
    /// live entirely in the workspace-keying and SQLite layers; they do not
    /// depend on `run_wallet_sync_with_retry` advancing `fully_scanned_height`.
    mod cancel_resume {
        use std::sync::Arc;

        use secrecy::SecretString;
        use tokio::sync::Mutex;
        use zcash_client_backend::data_api::{chain::ChainState, AccountBirthday, WalletRead};
        use zcash_client_sqlite::{util::SystemClock, WalletDb};
        use zcash_primitives::block::BlockHash;
        use zcash_protocol::consensus::BlockHeight;

        use super::super::{import_accounts, ScanTaskState};
        use crate::{
            derivation::{derive_accounts, legacy_transparent_account_key, mnemonic_seed},
            models::{RuntimeScanConfig, ScanHandle, ZeckNetwork},
            workspace::{consensus_network, RecoveryWorkspace},
        };

        const TEST_SEED: &str = "abandon abandon abandon abandon abandon abandon \
                                  abandon abandon abandon abandon abandon abandon \
                                  abandon abandon abandon abandon abandon abandon \
                                  abandon abandon abandon abandon abandon art";

        fn test_config(data_dir: std::path::PathBuf) -> RuntimeScanConfig {
            RuntimeScanConfig {
                seed_phrase: SecretString::new(TEST_SEED.to_owned()),
                birthday: 419_200,
                num_accounts: Some(2),
                gap_limit: 5,
                lightwalletd_url: "https://example.invalid:443".to_owned(),
                data_dir,
                network: ZeckNetwork::Mainnet,
            }
        }

        fn test_birthday() -> AccountBirthday {
            // Sapling activation is at 419200; the prior chain state is block 419199.
            // ChainState::empty sets empty commitment trees — valid for a scan
            // that doesn't need real note data (account-import idempotency tests).
            AccountBirthday::from_parts(
                ChainState::empty(BlockHeight::from_u32(419_199), BlockHash([0u8; 32])),
                None,
            )
        }

        #[tokio::test]
        async fn wallet_db_persists_after_workspace_handle_is_dropped() {
            let tempdir = tempfile::tempdir().expect("temp dir");
            let config = test_config(tempdir.path().to_owned());
            let workspace = RecoveryWorkspace::from_runtime(&config).expect("workspace");
            let seed = mnemonic_seed(&config.seed_phrase).expect("seed");
            workspace.initialize(config.network, &seed).expect("workspace.initialize");
            let transparent_account =
                legacy_transparent_account_key(&config.seed_phrase, config.network)
                    .expect("transparent account key");
            let accounts =
                derive_accounts(&config.seed_phrase, config.network, 2).expect("accounts");
            let state = Arc::new(Mutex::new(ScanTaskState::new(ScanHandle::new())));

            import_accounts(
                &workspace,
                config.network,
                &seed,
                &test_birthday(),
                &transparent_account,
                &accounts,
                &state,
            )
            .await
            .expect("import_accounts should succeed");

            let db_path = workspace.wallet_db_path().to_owned();
            // Simulated abort: drop all in-memory state.
            drop(workspace);
            drop(state);

            assert!(
                db_path.exists(),
                "wallet DB must persist on disk after the workspace handle is dropped (resume contract)"
            );
        }

        #[tokio::test]
        async fn resume_reuses_same_workspace_and_does_not_duplicate_accounts() {
            let tempdir = tempfile::tempdir().expect("temp dir");
            let config = test_config(tempdir.path().to_owned());
            let seed = mnemonic_seed(&config.seed_phrase).expect("seed");
            let transparent_account =
                legacy_transparent_account_key(&config.seed_phrase, config.network)
                    .expect("transparent account key");
            let accounts =
                derive_accounts(&config.seed_phrase, config.network, 2).expect("accounts");
            let state = Arc::new(Mutex::new(ScanTaskState::new(ScanHandle::new())));

            // ── First scan pass: import 2 accounts then simulate abort ──────────
            let workspace1 = RecoveryWorkspace::from_runtime(&config).expect("workspace");
            workspace1.initialize(config.network, &seed).expect("workspace.initialize");
            import_accounts(
                &workspace1,
                config.network,
                &seed,
                &test_birthday(),
                &transparent_account,
                &accounts,
                &state,
            )
            .await
            .expect("first import_accounts should succeed");

            let root1 = workspace1.root().to_owned();
            let db_path = workspace1.wallet_db_path().to_owned();
            drop(workspace1);

            // ── Resume: same config must resolve to the same workspace ───────────
            let workspace2 = RecoveryWorkspace::from_runtime(&config).expect("workspace (resume)");
            assert_eq!(
                workspace2.root(),
                root1,
                "resume must reuse the same workspace directory"
            );
            workspace2.initialize(config.network, &seed).expect("workspace2.initialize");

            // Re-importing the same accounts must be idempotent.
            import_accounts(
                &workspace2,
                config.network,
                &seed,
                &test_birthday(),
                &transparent_account,
                &accounts,
                &state,
            )
            .await
            .expect("resume import_accounts should succeed");

            // Open the DB and verify account count is still 2, not 4.
            let wallet_db = WalletDb::for_path(
                db_path,
                consensus_network(config.network),
                SystemClock,
                rand_core::OsRng,
            )
            .expect("WalletDb::for_path should succeed");

            let account_ids = wallet_db
                .get_account_ids()
                .expect("get_account_ids should succeed");
            assert_eq!(
                account_ids.len(),
                2,
                "re-importing the same 2 accounts must yield exactly 2 in the DB (not 4)"
            );
        }
    }
}
