use std::{collections::HashMap, sync::Arc};

use secrecy::SecretString;
use tokio::sync::RwLock;

use crate::{
    address::validate_destination_address,
    error::{ZeckError, ZeckResult},
    models::{
        RuntimeScanConfig, ScanConfig, ScanHandle, ScanProgress, SweepProposal, TxBroadcastResult,
    },
    scan::{run_preview_scan, ScanTaskState, SharedScanTaskState},
};

#[derive(Clone, Default)]
pub struct RecoveryService {
    scans: Arc<RwLock<HashMap<String, SharedScanTaskState>>>,
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
        if config.gap_limit == 0 {
            return Err(ZeckError::InvalidConfig(
                "gap limit must be at least 1".to_owned(),
            ));
        }

        let handle = ScanHandle::new();
        let state = Arc::new(tokio::sync::Mutex::new(ScanTaskState::new(handle.clone())));

        self.scans
            .write()
            .await
            .insert(handle.id.clone(), state.clone());

        let runtime = RuntimeScanConfig {
            seed_phrase,
            birthday: config.birthday,
            num_accounts: config.num_accounts,
            gap_limit: config.gap_limit,
            lightwalletd_url: config.lightwalletd_url,
            network: config.network,
        };

        tokio::spawn(run_preview_scan(state, runtime));

        Ok(handle)
    }

    pub async fn get_scan_progress(&self, handle: &ScanHandle) -> ZeckResult<ScanProgress> {
        let state = self
            .scans
            .read()
            .await
            .get(&handle.id)
            .cloned()
            .ok_or(ZeckError::UnknownScanHandle)?;

        let progress = state.lock().await.progress.clone();
        Ok(progress)
    }

    pub async fn cancel_scan(&self, handle: &ScanHandle) -> ZeckResult<()> {
        let state = self
            .scans
            .read()
            .await
            .get(&handle.id)
            .cloned()
            .ok_or(ZeckError::UnknownScanHandle)?;

        state
            .lock()
            .await
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);

        Ok(())
    }

    pub async fn propose_sweep(
        &self,
        handle: &ScanHandle,
        destination: &str,
    ) -> ZeckResult<SweepProposal> {
        let progress = self.get_scan_progress(handle).await?;
        let destination = validate_destination_address(destination)?;

        Ok(SweepProposal {
            transactions: progress
                .accounts
                .iter()
                .filter(|account| account.total_zatoshis > 0)
                .map(|account| crate::models::ProposedTx {
                    source_account: account.account_index,
                    destination: destination.encoded.clone(),
                    gross_zatoshis: account.total_zatoshis,
                    fee_zatoshis: 0,
                    net_zatoshis: account.total_zatoshis,
                    note: "Sweep preview only. Broadcasting is not implemented yet.".to_owned(),
                })
                .collect(),
            total_send_zatoshis: 0,
            total_fee_zatoshis: 0,
            net_received_zatoshis: 0,
            dry_run_default: true,
            warning: Some(
                "Sweep proposal is currently a placeholder. ZECK validates the destination but does not sign or broadcast transactions yet."
                    .to_owned(),
            ),
        })
    }

    pub async fn execute_sweep(
        &self,
        _handle: &ScanHandle,
        _destination: &str,
    ) -> ZeckResult<Vec<TxBroadcastResult>> {
        Err(ZeckError::SweepNotImplemented)
    }
}
