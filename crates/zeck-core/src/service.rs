use std::{collections::HashMap, convert::Infallible, sync::Arc, time::Instant};

use secrecy::SecretString;
use tokio::{
    sync::{Mutex, RwLock},
    task::JoinHandle,
    time::Duration,
};
use zcash_address::ZcashAddress;
use zcash_client_backend::{
    data_api::{
        wallet::{
            create_proposed_transactions, input_selection::GreedyInputSelector,
            propose_send_max_transfer, propose_shielding, ConfirmationsPolicy, SpendingKeys,
        },
        MaxSpendMode, TransactionStatus, WalletRead, WalletWrite,
    },
    fees::{standard::SingleOutputChangeStrategy, DustOutputPolicy, StandardFeeRule},
    proto::service::{
        compact_tx_streamer_client::CompactTxStreamerClient, RawTransaction, TxFilter,
    },
    wallet::OvkPolicy,
};
use zcash_client_sqlite::{util::SystemClock, WalletDb};
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_primitives::transaction::{fees::zip317::MINIMUM_FEE, TxId};
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::{consensus::BlockHeight, memo::MemoBytes, value::Zatoshis, ShieldedProtocol};

use crate::{
    address::validate_destination_address,
    derivation::{legacy_transparent_account_key, legacy_transparent_secret_key, mnemonic_seed},
    error::{ZeckError, ZeckResult},
    lightwalletd::{connect_lightwalletd_endpoints, parse_lightwalletd_endpoints},
    models::{
        ProposedTx, ProposedTxKind, RuntimeScanConfig, ScanConfig, ScanHandle, ScanPhase,
        ScanProgress, SkippedSweepAccount, SweepProposal, SweepRequest, TxBroadcastResult,
    },
    scan::{
        refresh_scan_progress, run_recovery_scan, run_wallet_sync, ScanTaskState,
        SharedScanTaskState, TrackedAccount,
    },
    workspace::{consensus_network, RecoveryWorkspace},
};

const RECOVERY_MEMO_DEFAULT: &str = "ZECK recovery";
const SESSION_RETENTION_SECS: u64 = 300;
const CONFIRMATION_POLL_INTERVAL_SECS: u64 = 5;
const CONFIRMATION_POLL_ATTEMPTS: u32 = 24;

struct ScanSession {
    state: SharedScanTaskState,
    runtime: RuntimeScanConfig,
    started_at: Instant,
    task: Mutex<Option<JoinHandle<()>>>,
}

type SharedScanSession = Arc<ScanSession>;

#[derive(Clone, Default)]
pub struct RecoveryService {
    sessions: Arc<RwLock<HashMap<String, SharedScanSession>>>,
}

impl RecoveryService {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn start_scan(
        &self,
        config: ScanConfig,
        seed_phrase: SecretString,
    ) -> ZeckResult<ScanHandle> {
        validate_scan_config(&config)?;

        let handle = ScanHandle::new();
        let state = Arc::new(tokio::sync::Mutex::new(ScanTaskState::new(handle.clone())));
        let runtime = RuntimeScanConfig {
            seed_phrase,
            birthday: config.birthday,
            num_accounts: config.num_accounts,
            gap_limit: config.gap_limit,
            lightwalletd_url: config.lightwalletd_url,
            data_dir: config.data_dir,
            network: config.network,
        };
        let session = Arc::new(ScanSession {
            state: state.clone(),
            runtime: runtime.clone(),
            started_at: Instant::now(),
            task: Mutex::new(None),
        });

        self.sessions
            .write()
            .await
            .insert(handle.id.clone(), session.clone());

        let sessions = self.sessions.clone();
        let handle_id = handle.id.clone();
        let task = tokio::spawn(async move {
            run_recovery_scan(state, runtime).await;
            spawn_session_cleanup(sessions, handle_id);
        });
        *session.task.lock().await = Some(task);

        Ok(handle)
    }

    pub async fn get_scan_progress(&self, handle: &ScanHandle) -> ZeckResult<ScanProgress> {
        let session = self.session(handle).await?;
        let mut progress = session.state.lock().await.progress.clone();
        let elapsed_seconds = session.started_at.elapsed().as_secs();
        progress.elapsed_seconds = Some(elapsed_seconds);
        progress.estimated_remaining_seconds =
            estimate_remaining_seconds(&progress, elapsed_seconds);
        Ok(progress)
    }

    pub async fn cancel_scan(&self, handle: &ScanHandle) -> ZeckResult<()> {
        let session = self.session(handle).await?;

        {
            let state = session.state.lock().await;
            state
                .cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        {
            let mut state = session.state.lock().await;
            state.progress.phase = ScanPhase::Cancelled;
            state.progress.message = Some("Recovery scan cancelled.".to_owned());
        }
        if let Some(task) = session.task.lock().await.take() {
            task.abort();
        }
        spawn_session_cleanup(self.sessions.clone(), handle.id.clone());
        Ok(())
    }

    pub async fn propose_sweep(
        &self,
        handle: &ScanHandle,
        request: SweepRequest,
    ) -> ZeckResult<SweepProposal> {
        let progress = self.get_scan_progress(handle).await?;
        if progress.phase != ScanPhase::Complete {
            return Err(ZeckError::ScanNotReady(format!(
                "current phase is {:?}",
                progress.phase
            )));
        }
        build_sweep_proposal(&progress, request)
    }

    pub async fn execute_sweep(
        &self,
        handle: &ScanHandle,
        request: SweepRequest,
    ) -> ZeckResult<Vec<TxBroadcastResult>> {
        let session = self.session(handle).await?;
        let progress = session.state.lock().await.progress.clone();
        if progress.phase != ScanPhase::Complete {
            return Err(ZeckError::ScanNotReady(format!(
                "current phase is {:?}",
                progress.phase
            )));
        }

        let _ = build_sweep_proposal(&progress, request.clone())?;
        execute_sweep_for_session(session, request).await
    }

    async fn session(&self, handle: &ScanHandle) -> ZeckResult<SharedScanSession> {
        self.sessions
            .read()
            .await
            .get(&handle.id)
            .cloned()
            .ok_or(ZeckError::UnknownScanHandle)
    }
}

fn validate_scan_config(config: &ScanConfig) -> ZeckResult<()> {
    if config.gap_limit == 0 {
        return Err(ZeckError::InvalidConfig(
            "gap limit must be at least 1".to_owned(),
        ));
    }
    if config.gap_limit > 500 {
        return Err(ZeckError::InvalidConfig(
            "gap limit must not exceed 500".to_owned(),
        ));
    }
    if matches!(config.num_accounts, Some(0)) {
        return Err(ZeckError::InvalidConfig(
            "num_accounts must be at least 1".to_owned(),
        ));
    }
    if let Some(num_accounts) = config.num_accounts {
        if num_accounts > 500 {
            return Err(ZeckError::InvalidConfig(
                "num_accounts must not exceed 500".to_owned(),
            ));
        }
    }
    if parse_lightwalletd_endpoints(&config.lightwalletd_url).is_empty() {
        return Err(ZeckError::InvalidConfig(
            "at least one lightwalletd endpoint is required".to_owned(),
        ));
    }

    Ok(())
}

fn spawn_session_cleanup(
    sessions: Arc<RwLock<HashMap<String, SharedScanSession>>>,
    handle_id: String,
) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(SESSION_RETENTION_SECS)).await;
        sessions.write().await.remove(&handle_id);
    });
}

fn build_sweep_proposal(
    progress: &ScanProgress,
    request: SweepRequest,
) -> ZeckResult<SweepProposal> {
    let destination = validate_destination_address(&request.destination)?;
    let memo = normalized_memo_text(request.memo.as_deref())?;
    let minimum_fee_zatoshis = u64::from(MINIMUM_FEE);

    let mut transactions = Vec::new();
    let mut skipped_accounts = Vec::new();
    let mut total_fee_zatoshis = 0u64;
    let mut total_send_zatoshis = 0u64;
    let mut net_received_zatoshis = 0u64;

    for account in progress
        .accounts
        .iter()
        .filter(|account| account.total_zatoshis > 0)
    {
        let shielded_existing = account.sapling_zatoshis + account.orchard_zatoshis;
        let mut shielded_available = shielded_existing;

        if account.transparent_zatoshis > 0 {
            if account.transparent_zatoshis <= minimum_fee_zatoshis {
                skipped_accounts.push(SkippedSweepAccount {
                    account_index: account.account_index,
                    gross_zatoshis: account.transparent_zatoshis,
                    reason: format!(
                        "Transparent balance is too small to cover the ZIP 317 shielding fee floor of {minimum_fee_zatoshis} zats."
                    ),
                });
            } else {
                let shielding_fee_zatoshis = minimum_fee_zatoshis;
                let shielded_after_step_one = account.transparent_zatoshis - shielding_fee_zatoshis;
                shielded_available = shielded_available
                    .checked_add(shielded_after_step_one)
                    .ok_or_else(|| {
                        ZeckError::Internal(
                            "sweep proposal overflowed the supported range".to_owned(),
                        )
                    })?;
                total_send_zatoshis = total_send_zatoshis
                    .checked_add(account.transparent_zatoshis)
                    .ok_or_else(|| {
                        ZeckError::Internal(
                            "sweep proposal overflowed the supported range".to_owned(),
                        )
                    })?;
                total_fee_zatoshis = total_fee_zatoshis
                    .checked_add(shielding_fee_zatoshis)
                    .ok_or_else(|| {
                        ZeckError::Internal(
                            "sweep proposal overflowed the supported range".to_owned(),
                        )
                    })?;

                transactions.push(ProposedTx {
                    kind: ProposedTxKind::ShieldTransparent,
                    source_account: account.account_index,
                    destination: account.unified_address.clone(),
                    gross_zatoshis: account.transparent_zatoshis,
                    fee_zatoshis: shielding_fee_zatoshis,
                    net_zatoshis: shielded_after_step_one,
                    note: format!(
                        "Estimated shielding step for {} transparent UTXOs before the external sweep.",
                        account.transparent_utxo_count
                    ),
                    memo: None,
                });
            }
        }

        if shielded_available == 0 {
            continue;
        }
        if shielded_available <= minimum_fee_zatoshis {
            skipped_accounts.push(SkippedSweepAccount {
                account_index: account.account_index,
                gross_zatoshis: shielded_available,
                reason: format!(
                    "Shielded balance is too small to cover the ZIP 317 sweep fee floor of {minimum_fee_zatoshis} zats."
                ),
            });
            continue;
        }

        let sweep_fee_zatoshis = minimum_fee_zatoshis;
        let net_received_for_account = shielded_available - sweep_fee_zatoshis;
        total_send_zatoshis = total_send_zatoshis
            .checked_add(shielded_available)
            .ok_or_else(|| {
                ZeckError::Internal("sweep proposal overflowed the supported range".to_owned())
            })?;
        total_fee_zatoshis = total_fee_zatoshis
            .checked_add(sweep_fee_zatoshis)
            .ok_or_else(|| {
                ZeckError::Internal("sweep proposal overflowed the supported range".to_owned())
            })?;
        net_received_zatoshis = net_received_zatoshis
            .checked_add(net_received_for_account)
            .ok_or_else(|| {
                ZeckError::Internal("sweep proposal overflowed the supported range".to_owned())
            })?;

        transactions.push(ProposedTx {
            kind: ProposedTxKind::SweepShielded,
            source_account: account.account_index,
            destination: destination.encoded.clone(),
            gross_zatoshis: shielded_available,
            fee_zatoshis: sweep_fee_zatoshis,
            net_zatoshis: net_received_for_account,
            note: if shielded_existing > 0 && account.transparent_zatoshis > 0 {
                "Estimated external recovery sweep after shielding the transparent portion and combining it with existing shielded funds."
                    .to_owned()
            } else if shielded_existing > 0 {
                "Estimated external recovery sweep for shielded funds already tracked in this account."
                    .to_owned()
            } else {
                "Estimated external recovery sweep after shielding completes.".to_owned()
            },
            memo: Some(memo.clone()),
        });
    }

    if let Some(max_fee_zatoshis) = request.max_fee_zatoshis {
        if total_fee_zatoshis > max_fee_zatoshis {
            return Err(ZeckError::MaxFeeExceeded(format!(
                "estimated fee {} zats exceeds limit {} zats",
                total_fee_zatoshis, max_fee_zatoshis
            )));
        }
    }

    let warning = if net_received_zatoshis > 0 {
        "This dry-run proposal uses the authoritative scan balances from the persisted wallet workspace. ZECK estimates any required shielding first, then a final sweep to the destination Unified Address."
            .to_owned()
    } else if !skipped_accounts.is_empty() {
        "Balances were detected, but every discovered account was skipped because the ZIP 317 fee floor would consume the recoverable value."
            .to_owned()
    } else {
        "No spendable balances were found in the completed scan.".to_owned()
    };

    Ok(SweepProposal {
        transactions,
        skipped_accounts,
        total_send_zatoshis,
        total_fee_zatoshis,
        net_received_zatoshis,
        dry_run_default: true,
        warning: Some(warning),
    })
}

async fn execute_sweep_for_session(
    session: SharedScanSession,
    request: SweepRequest,
) -> ZeckResult<Vec<TxBroadcastResult>> {
    let destination = validate_destination_address(&request.destination)?;
    let memo_text = normalized_memo_text(request.memo.as_deref())?;
    let memo_bytes = if memo_text == RECOVERY_MEMO_DEFAULT {
        Some(MemoBytes::from_bytes(memo_text.as_bytes()).map_err(|err| {
            ZeckError::InvalidMemo(format!("default recovery memo could not be encoded: {err}"))
        })?)
    } else {
        Some(
            MemoBytes::from_bytes(memo_text.as_bytes())
                .map_err(|err| ZeckError::InvalidMemo(err.to_string()))?,
        )
    };

    let (runtime, workspace, tracked_accounts, progress) = {
        let guard = session.state.lock().await;
        let workspace = guard
            .workspace
            .clone()
            .ok_or_else(|| ZeckError::ScanNotReady("wallet workspace is unavailable".to_owned()))?;
        (
            session.runtime.clone(),
            workspace,
            guard.tracked_accounts.clone(),
            guard.progress.clone(),
        )
    };

    let seed = mnemonic_seed(&runtime.seed_phrase)?;
    let transparent_account =
        legacy_transparent_account_key(&runtime.seed_phrase, runtime.network)?;
    let network = consensus_network(runtime.network);
    let destination_address =
        ZcashAddress::try_from_encoded(&destination.encoded).map_err(|err| {
            ZeckError::InvalidAddress(format!(
                "failed to decode destination Unified Address: {err}"
            ))
        })?;
    let prover = LocalTxProver::bundled();
    let mut total_fee_zatoshis = 0u64;
    let mut results = Vec::new();

    let preferred_endpoint = progress
        .server
        .as_ref()
        .map(|server| server.endpoint.as_str());
    let (mut client, _) =
        connect_lightwalletd_endpoints(&runtime.lightwalletd_url, preferred_endpoint).await?;

    run_wallet_sync(&workspace, &network, &mut client).await?;
    refresh_scan_progress(
        &session.state,
        &workspace,
        runtime.network,
        runtime.birthday.min(chain_tip_height(&mut client).await?),
    )
    .await?;

    for tracked_account in tracked_accounts {
        if account_total_zatoshis(
            &workspace,
            runtime.network,
            tracked_account.wallet_account_id,
        )? == 0
        {
            continue;
        }

        let zip32_index =
            zip32::AccountId::try_from(tracked_account.derived.index).map_err(|_| {
                ZeckError::InvalidConfig(format!(
                    "account index {} is out of range",
                    tracked_account.derived.index
                ))
            })?;
        let usk = UnifiedSpendingKey::from_seed(&network, &seed, zip32_index).map_err(|err| {
            ZeckError::Wallet(format!(
                "deriving account {}: {err}",
                tracked_account.derived.index
            ))
        })?;

        let transparent_balance =
            account_transparent_zatoshis(&workspace, runtime.network, &tracked_account)?;
        if transparent_balance > 0 {
            let fee = execute_shielding_step(
                &workspace,
                runtime.network,
                &mut client,
                &tracked_account,
                &transparent_account,
                &usk,
                &prover,
                &mut results,
            )
            .await?;
            total_fee_zatoshis = total_fee_zatoshis.checked_add(fee).ok_or_else(|| {
                ZeckError::Internal("fee total overflowed the supported range".to_owned())
            })?;
            enforce_max_fee(total_fee_zatoshis, request.max_fee_zatoshis)?;

            if !last_account_confirmed(&results, tracked_account.derived.index) {
                continue;
            }

            run_wallet_sync(&workspace, &network, &mut client).await?;
            refresh_scan_progress(
                &session.state,
                &workspace,
                runtime.network,
                runtime.birthday.min(chain_tip_height(&mut client).await?),
            )
            .await?;
        }

        let fee = execute_send_max_step(
            &workspace,
            runtime.network,
            &mut client,
            &tracked_account,
            &usk,
            &destination_address,
            memo_bytes.clone(),
            &prover,
            &mut results,
        )
        .await?;
        total_fee_zatoshis = total_fee_zatoshis.checked_add(fee).ok_or_else(|| {
            ZeckError::Internal("fee total overflowed the supported range".to_owned())
        })?;
        enforce_max_fee(total_fee_zatoshis, request.max_fee_zatoshis)?;
    }

    Ok(results)
}

async fn execute_shielding_step(
    workspace: &RecoveryWorkspace,
    network: crate::models::ZeckNetwork,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    tracked_account: &TrackedAccount,
    transparent_account: &zcash_transparent::keys::AccountPrivKey,
    usk: &UnifiedSpendingKey,
    prover: &LocalTxProver,
    results: &mut Vec<TxBroadcastResult>,
) -> ZeckResult<u64> {
    let mut wallet_db = WalletDb::for_path(
        workspace.wallet_db_path(),
        consensus_network(network),
        SystemClock,
        rand_core::OsRng,
    )
    .map_err(|err| {
        ZeckError::Storage(format!(
            "opening wallet database {}: {err}",
            workspace.wallet_db_path().display()
        ))
    })?;
    let input_selector = GreedyInputSelector::<_>::new();
    let change_strategy = SingleOutputChangeStrategy::<_>::new(
        StandardFeeRule::Zip317,
        None,
        ShieldedProtocol::Orchard,
        DustOutputPolicy::default(),
    );

    let proposal = propose_shielding::<_, _, _, _, Infallible>(
        &mut wallet_db,
        &consensus_network(network),
        &input_selector,
        &change_strategy,
        Zatoshis::ZERO,
        &tracked_account.transparent_receivers,
        tracked_account.wallet_account_id,
        ConfirmationsPolicy::MIN,
    )
    .map_err(|err| ZeckError::TransactionBuild(format!("building shielding proposal: {err}")))?;
    let fee_zatoshis = proposal_fee_zatoshis(&proposal)?;

    let mut standalone_keys = HashMap::new();
    standalone_keys.insert(
        tracked_account.transparent_receivers[0],
        legacy_transparent_secret_key(
            transparent_account,
            crate::models::AddressScope::External,
            tracked_account.derived.index,
        )?,
    );
    standalone_keys.insert(
        tracked_account.transparent_receivers[1],
        legacy_transparent_secret_key(
            transparent_account,
            crate::models::AddressScope::Internal,
            tracked_account.derived.index,
        )?,
    );
    let txids = create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
        &mut wallet_db,
        &consensus_network(network),
        prover,
        prover,
        &SpendingKeys::new(usk.clone(), standalone_keys),
        OvkPolicy::Sender,
        &proposal,
    )
    .map_err(|err| ZeckError::TransactionBuild(format!("creating shielding transaction: {err}")))?;

    broadcast_transactions(
        &mut wallet_db,
        client,
        tracked_account.derived.index,
        txids.into_iter().collect(),
        "shielding",
        results,
    )
    .await?;

    Ok(fee_zatoshis)
}

async fn execute_send_max_step(
    workspace: &RecoveryWorkspace,
    network: crate::models::ZeckNetwork,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    tracked_account: &TrackedAccount,
    usk: &UnifiedSpendingKey,
    destination_address: &ZcashAddress,
    memo_bytes: Option<MemoBytes>,
    prover: &LocalTxProver,
    results: &mut Vec<TxBroadcastResult>,
) -> ZeckResult<u64> {
    let mut wallet_db = WalletDb::for_path(
        workspace.wallet_db_path(),
        consensus_network(network),
        SystemClock,
        rand_core::OsRng,
    )
    .map_err(|err| {
        ZeckError::Storage(format!(
            "opening wallet database {}: {err}",
            workspace.wallet_db_path().display()
        ))
    })?;

    let proposal = propose_send_max_transfer::<_, _, _, Infallible>(
        &mut wallet_db,
        &consensus_network(network),
        tracked_account.wallet_account_id,
        &[ShieldedProtocol::Sapling, ShieldedProtocol::Orchard],
        &StandardFeeRule::Zip317,
        destination_address.clone(),
        memo_bytes,
        MaxSpendMode::MaxSpendable,
        ConfirmationsPolicy::MIN,
    )
    .map_err(|err| ZeckError::TransactionBuild(format!("building sweep proposal: {err}")))?;
    let fee_zatoshis = proposal_fee_zatoshis(&proposal)?;

    let txids = create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
        &mut wallet_db,
        &consensus_network(network),
        prover,
        prover,
        &SpendingKeys::from_unified_spending_key(usk.clone()),
        OvkPolicy::Sender,
        &proposal,
    )
    .map_err(|err| ZeckError::TransactionBuild(format!("creating sweep transaction: {err}")))?;

    broadcast_transactions(
        &mut wallet_db,
        client,
        tracked_account.derived.index,
        txids.into_iter().collect(),
        "sweep",
        results,
    )
    .await?;

    Ok(fee_zatoshis)
}

async fn broadcast_transactions(
    wallet_db: &mut WalletDb<
        rusqlite::Connection,
        zcash_protocol::consensus::Network,
        SystemClock,
        rand_core::OsRng,
    >,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    account_index: u32,
    txids: Vec<TxId>,
    label: &str,
    results: &mut Vec<TxBroadcastResult>,
) -> ZeckResult<()> {
    for txid in txids {
        let tx = wallet_db
            .get_transaction(txid)
            .map_err(|err| ZeckError::Wallet(format!("loading {label} transaction {txid}: {err}")))?
            .ok_or_else(|| {
                ZeckError::Wallet(format!(
                    "wallet did not persist the {label} transaction {txid}"
                ))
            })?;

        let mut tx_bytes = Vec::new();
        tx.write(&mut tx_bytes).map_err(|err| {
            ZeckError::TransactionBuild(format!("serializing transaction {txid}: {err}"))
        })?;

        let response = client
            .send_transaction(RawTransaction {
                data: tx_bytes,
                height: 0,
            })
            .await
            .map_err(|err| ZeckError::Broadcast(err.to_string()))?
            .into_inner();
        if response.error_code != 0 {
            return Err(ZeckError::Broadcast(format!(
                "{label} transaction {txid} was rejected: {}",
                response.error_message
            )));
        }

        let (status, detail, confirmed_height) =
            wait_for_confirmation(wallet_db, client, txid, label).await?;
        results.push(TxBroadcastResult {
            source_account: account_index,
            txid: Some(txid.to_string()),
            status,
            detail,
            confirmed_height,
        });
    }

    Ok(())
}

async fn wait_for_confirmation(
    wallet_db: &mut WalletDb<
        rusqlite::Connection,
        zcash_protocol::consensus::Network,
        SystemClock,
        rand_core::OsRng,
    >,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    txid: TxId,
    label: &str,
) -> ZeckResult<(String, String, Option<u32>)> {
    for _ in 0..CONFIRMATION_POLL_ATTEMPTS {
        match client
            .get_transaction(TxFilter {
                block: None,
                index: 0,
                hash: txid.as_ref().to_vec(),
            })
            .await
        {
            Ok(response) => {
                let tx = response.into_inner();
                if tx.height == 0 {
                    wallet_db
                        .set_transaction_status(txid, TransactionStatus::NotInMainChain)
                        .map_err(|err| {
                            ZeckError::Wallet(format!(
                                "marking pending transaction {txid} in wallet db: {err}"
                            ))
                        })?;
                } else if tx.height == u64::MAX {
                    wallet_db
                        .set_transaction_status(txid, TransactionStatus::NotInMainChain)
                        .map_err(|err| {
                            ZeckError::Wallet(format!(
                                "marking reorged transaction {txid} in wallet db: {err}"
                            ))
                        })?;
                } else {
                    let mined_height = u32::try_from(tx.height).map_err(|_| {
                        ZeckError::Broadcast(format!(
                            "{label} transaction {txid} returned an invalid mined height"
                        ))
                    })?;
                    wallet_db
                        .set_transaction_status(
                            txid,
                            TransactionStatus::Mined(BlockHeight::from_u32(mined_height)),
                        )
                        .map_err(|err| {
                            ZeckError::Wallet(format!(
                                "marking mined transaction {txid} in wallet db: {err}"
                            ))
                        })?;
                    return Ok((
                        "confirmed".to_owned(),
                        format!("{label} transaction mined at height {mined_height}."),
                        Some(mined_height),
                    ));
                }
            }
            Err(_) => {
                wallet_db
                    .set_transaction_status(txid, TransactionStatus::NotInMainChain)
                    .map_err(|err| {
                        ZeckError::Wallet(format!(
                            "marking pending transaction {txid} in wallet db: {err}"
                        ))
                    })?;
            }
        }

        tokio::time::sleep(Duration::from_secs(CONFIRMATION_POLL_INTERVAL_SECS)).await;
    }

    Ok((
        "pending".to_owned(),
        format!(
            "{label} transaction broadcast successfully, but confirmation was not observed during the wait window."
        ),
        None,
    ))
}

async fn chain_tip_height(
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
) -> ZeckResult<u32> {
    let info = client
        .get_lightd_info(zcash_client_backend::proto::service::Empty {})
        .await
        .map_err(|err| ZeckError::Lightwalletd(err.to_string()))?
        .into_inner();
    u32::try_from(info.block_height)
        .map_err(|_| ZeckError::Lightwalletd("chain tip height overflowed u32".to_owned()))
}

fn account_total_zatoshis(
    workspace: &RecoveryWorkspace,
    network: crate::models::ZeckNetwork,
    account_id: zcash_client_sqlite::AccountUuid,
) -> ZeckResult<u64> {
    let wallet_db = WalletDb::for_path(
        workspace.wallet_db_path(),
        consensus_network(network),
        SystemClock,
        rand_core::OsRng,
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
        .ok_or_else(|| ZeckError::Wallet("wallet summary is unavailable".to_owned()))?;
    Ok(summary
        .account_balances()
        .get(&account_id)
        .map(|balance| u64::from(balance.total()))
        .unwrap_or(0))
}

fn account_transparent_zatoshis(
    workspace: &RecoveryWorkspace,
    network: crate::models::ZeckNetwork,
    tracked_account: &TrackedAccount,
) -> ZeckResult<u64> {
    let wallet_db = WalletDb::for_path(
        workspace.wallet_db_path(),
        consensus_network(network),
        SystemClock,
        rand_core::OsRng,
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
        .ok_or_else(|| ZeckError::Wallet("wallet summary is unavailable".to_owned()))?;
    Ok(summary
        .account_balances()
        .get(&tracked_account.wallet_account_id)
        .map(|balance| u64::from(balance.unshielded_balance().total()))
        .unwrap_or(0))
}

fn proposal_fee_zatoshis<NoteRef>(
    proposal: &zcash_client_backend::proposal::Proposal<StandardFeeRule, NoteRef>,
) -> ZeckResult<u64> {
    proposal.steps().iter().try_fold(0u64, |sum, step| {
        sum.checked_add(u64::from(step.balance().fee_required()))
            .ok_or_else(|| {
                ZeckError::Internal("fee total overflowed the supported range".to_owned())
            })
    })
}

fn enforce_max_fee(total_fee_zatoshis: u64, max_fee_zatoshis: Option<u64>) -> ZeckResult<()> {
    if let Some(max_fee_zatoshis) = max_fee_zatoshis {
        if total_fee_zatoshis > max_fee_zatoshis {
            return Err(ZeckError::MaxFeeExceeded(format!(
                "actual fee {} zats exceeds limit {} zats",
                total_fee_zatoshis, max_fee_zatoshis
            )));
        }
    }

    Ok(())
}

fn last_account_confirmed(results: &[TxBroadcastResult], account_index: u32) -> bool {
    results
        .iter()
        .rev()
        .find(|result| result.source_account == account_index)
        .map(|result| result.status == "confirmed")
        .unwrap_or(true)
}

fn normalized_memo_text(memo: Option<&str>) -> ZeckResult<String> {
    let value = memo
        .map(str::trim)
        .filter(|memo| !memo.is_empty())
        .unwrap_or(RECOVERY_MEMO_DEFAULT);

    MemoBytes::from_bytes(value.as_bytes())
        .map_err(|err| ZeckError::InvalidMemo(err.to_string()))?;
    Ok(value.to_owned())
}

fn estimate_remaining_seconds(progress: &ScanProgress, elapsed_seconds: u64) -> Option<u64> {
    if progress.blocks_total == 0 {
        return None;
    }
    if progress.blocks_scanned >= progress.blocks_total {
        return Some(0);
    }
    if progress.blocks_scanned == 0 || elapsed_seconds == 0 {
        return None;
    }

    let remaining_blocks = progress
        .blocks_total
        .saturating_sub(progress.blocks_scanned);
    Some(
        remaining_blocks
            .saturating_mul(elapsed_seconds)
            .checked_div(progress.blocks_scanned)
            .unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use super::build_sweep_proposal;
    use crate::{
        derive_accounts,
        error::ZeckError,
        models::{
            AccountBalancePreview, ScanHandle, ScanPhase, ScanProgress, SweepRequest, ZeckNetwork,
        },
    };

    fn derived_destination() -> String {
        derive_accounts(
            &SecretString::new(
                "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"
                    .to_owned(),
            ),
            ZeckNetwork::Mainnet,
            1,
        )
        .expect("derived account")[0]
            .unified_address
            .clone()
    }

    fn progress_with_account(account: AccountBalancePreview) -> ScanProgress {
        ScanProgress {
            handle: ScanHandle::new(),
            phase: ScanPhase::Complete,
            blocks_scanned: 1,
            blocks_total: 1,
            elapsed_seconds: None,
            estimated_remaining_seconds: None,
            accounts: vec![account],
            summary: None,
            server: None,
            message: None,
            error: None,
        }
    }

    #[test]
    fn proposal_separates_shielding_from_shielded_sweeping() {
        let proposal = build_sweep_proposal(
            &progress_with_account(AccountBalancePreview {
                account_index: 0,
                sapling_address: "zs-test".to_owned(),
                unified_address: "u-test".to_owned(),
                transparent_receive_address: "t-recv".to_owned(),
                transparent_change_address: "t-change".to_owned(),
                transparent_utxo_count: 1,
                sapling_zatoshis: 40_000,
                orchard_zatoshis: 0,
                transparent_zatoshis: 30_000,
                total_zatoshis: 70_000,
                status: "ok".to_owned(),
            }),
            SweepRequest {
                destination: derived_destination(),
                memo: Some("recovery".to_owned()),
                max_fee_zatoshis: None,
            },
        )
        .expect("proposal should build");

        assert_eq!(proposal.transactions.len(), 2);
        assert_eq!(
            proposal.transactions[0].kind,
            crate::models::ProposedTxKind::ShieldTransparent
        );
        assert_eq!(proposal.transactions[0].gross_zatoshis, 30_000);
        assert_eq!(proposal.transactions[1].gross_zatoshis, 60_000);
    }

    #[test]
    fn proposal_rejects_max_fee_below_estimate() {
        let err = build_sweep_proposal(
            &progress_with_account(AccountBalancePreview {
                account_index: 0,
                sapling_address: "zs-test".to_owned(),
                unified_address: "u-test".to_owned(),
                transparent_receive_address: "t-recv".to_owned(),
                transparent_change_address: "t-change".to_owned(),
                transparent_utxo_count: 1,
                sapling_zatoshis: 0,
                orchard_zatoshis: 0,
                transparent_zatoshis: 50_000,
                total_zatoshis: 50_000,
                status: "ok".to_owned(),
            }),
            SweepRequest {
                destination: derived_destination(),
                memo: None,
                max_fee_zatoshis: Some(15_000),
            },
        )
        .expect_err("proposal should fail");

        assert!(matches!(err, ZeckError::MaxFeeExceeded(_)));
    }

    #[test]
    fn proposal_skips_dusty_transparent_only_accounts() {
        let proposal = build_sweep_proposal(
            &progress_with_account(AccountBalancePreview {
                account_index: 0,
                sapling_address: "zs-test".to_owned(),
                unified_address: "u-test".to_owned(),
                transparent_receive_address: "t-recv".to_owned(),
                transparent_change_address: "t-change".to_owned(),
                transparent_utxo_count: 1,
                sapling_zatoshis: 0,
                orchard_zatoshis: 0,
                transparent_zatoshis: 5_000,
                total_zatoshis: 5_000,
                status: "ok".to_owned(),
            }),
            SweepRequest {
                destination: derived_destination(),
                memo: None,
                max_fee_zatoshis: None,
            },
        )
        .expect("proposal should build");

        assert!(proposal.transactions.is_empty());
        assert_eq!(proposal.skipped_accounts.len(), 1);
    }
}
