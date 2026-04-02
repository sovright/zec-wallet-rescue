use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use rustls::crypto::ring::default_provider;
use tokio::{sync::Mutex, time::Duration};
use tracing::warn;
use zcash_client_backend::proto::service::{
    compact_tx_streamer_client::CompactTxStreamerClient, Empty,
};

use crate::{
    derivation::derive_accounts,
    error::{ZeckError, ZeckResult},
    models::{
        AccountBalancePreview, LightwalletdProbe, RuntimeScanConfig, ScanHandle, ScanPhase,
        ScanProgress, ScanSummary,
    },
};

#[derive(Debug)]
pub struct ScanTaskState {
    pub progress: ScanProgress,
    pub cancelled: Arc<AtomicBool>,
}

impl ScanTaskState {
    pub fn new(handle: ScanHandle) -> Self {
        Self {
            progress: ScanProgress {
                handle,
                phase: ScanPhase::Idle,
                blocks_scanned: 0,
                blocks_total: 0,
                accounts: vec![],
                summary: None,
                server: None,
                message: None,
                error: None,
            },
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
}

pub type SharedScanTaskState = Arc<Mutex<ScanTaskState>>;

pub async fn run_preview_scan(state: SharedScanTaskState, config: RuntimeScanConfig) {
    if let Err(err) = run_preview_scan_inner(state.clone(), config).await {
        let mut guard = state.lock().await;
        guard.progress.phase = ScanPhase::Error;
        guard.progress.error = Some(err.to_string());
    }
}

async fn run_preview_scan_inner(
    state: SharedScanTaskState,
    config: RuntimeScanConfig,
) -> ZeckResult<()> {
    {
        let mut guard = state.lock().await;
        guard.progress.phase = ScanPhase::ValidatingSeed;
        guard.progress.message = Some("Validating BIP-39 seed phrase.".to_owned());
    }

    tokio::time::sleep(Duration::from_millis(150)).await;
    check_cancelled(&state).await?;

    let account_count = config.num_accounts.unwrap_or(config.gap_limit.clamp(5, 50));

    {
        let mut guard = state.lock().await;
        guard.progress.phase = ScanPhase::DerivingKeys;
        guard.progress.blocks_total = account_count as u64;
        guard.progress.message = Some(format!(
            "Deriving {} ZecWallet Lite-compatible address slots.",
            account_count
        ));
    }

    let accounts = derive_accounts(&config.seed_phrase, config.network, account_count)?;
    check_cancelled(&state).await?;

    {
        let mut guard = state.lock().await;
        guard.progress.phase = ScanPhase::ProbingLightwalletd;
        guard.progress.message = Some(format!(
            "Connecting to {} and checking chain metadata.",
            config.lightwalletd_url
        ));
    }

    let probe = probe_lightwalletd(&config.lightwalletd_url).await?;

    {
        let mut guard = state.lock().await;
        if let Some(height) = probe.latest_block_height {
            guard.progress.blocks_total = height.saturating_sub(config.birthday as u64);
        }
        guard.progress.server = Some(probe);
    }

    {
        let mut guard = state.lock().await;
        guard.progress.phase = ScanPhase::Previewing;
        guard.progress.message = Some(
            "Building a non-authoritative recovery preview. Compact-block balance scanning is still pending."
                .to_owned(),
        );
    }

    for (position, account) in accounts.into_iter().enumerate() {
        check_cancelled(&state).await?;

        {
            let mut guard = state.lock().await;
            guard.progress.accounts.push(AccountBalancePreview {
                account_index: account.index,
                sapling_address: account.sapling_address,
                unified_address: account.unified_address,
                transparent_receive_address: account.transparent_receive_address,
                transparent_change_address: account.transparent_change_address,
                sapling_zatoshis: 0,
                orchard_zatoshis: 0,
                transparent_zatoshis: 0,
                total_zatoshis: 0,
                status: "Preview only".to_owned(),
            });
            guard.progress.blocks_scanned = (position as u64) + 1;
        }

        tokio::time::sleep(Duration::from_millis(35)).await;
    }

    {
        let mut guard = state.lock().await;
        guard.progress.phase = ScanPhase::Complete;
        guard.progress.summary = Some(ScanSummary {
            total_zatoshis: 0,
            authoritative_balances: false,
            note: "Key derivation and server probing are live. Full compact-block balance recovery and sweeping still need to be wired into this interface.".to_owned(),
        });
        guard.progress.message = Some(
            "Preview complete. Derived addresses are ready for manual inspection and future chain-sync integration."
                .to_owned(),
        );
    }

    Ok(())
}

async fn probe_lightwalletd(endpoint: &str) -> ZeckResult<LightwalletdProbe> {
    let _ = default_provider().install_default();
    let mut client = CompactTxStreamerClient::connect(endpoint.to_owned())
        .await
        .map_err(|err| ZeckError::Lightwalletd(err.to_string()))?;
    let response = client
        .get_lightd_info(Empty {})
        .await
        .map_err(|err| ZeckError::Lightwalletd(err.to_string()))?
        .into_inner();

    Ok(LightwalletdProbe {
        endpoint: endpoint.to_owned(),
        vendor: Some(response.vendor),
        chain_name: Some(response.chain_name),
        latest_block_height: Some(response.block_height as u64),
        sapling_activation_height: Some(response.sapling_activation_height as u64),
    })
}

async fn check_cancelled(state: &SharedScanTaskState) -> ZeckResult<()> {
    let cancelled = {
        let guard = state.lock().await;
        guard.cancelled.load(Ordering::SeqCst)
    };

    if cancelled {
        let mut guard = state.lock().await;
        guard.progress.phase = ScanPhase::Cancelled;
        guard.progress.message = Some("Recovery preview cancelled.".to_owned());
        warn!("scan {} cancelled", guard.progress.handle.id);
        return Err(ZeckError::Internal("scan cancelled".to_owned()));
    }

    Ok(())
}
