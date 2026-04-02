use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use zeck_core::{
    estimate_birthday_from_date as estimate_birthday, validate_destination_address,
    validate_mnemonic_words, RecoveryService, ScanConfig, ScanHandle, SweepProposal,
    TxBroadcastResult, ZeckNetwork,
};

#[derive(Clone)]
pub struct AppState {
    pub service: RecoveryService,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanConfigInput {
    pub seed: String,
    pub birthday: u32,
    pub num_accounts: Option<u32>,
    pub gap_limit: u32,
    pub lightwalletd_url: String,
    pub network: ZeckNetwork,
}

#[tauri::command]
pub async fn validate_seed(words: Vec<String>) -> Result<bool, String> {
    validate_mnemonic_words(&words)
        .map(|_| true)
        .map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn validate_address(address: String) -> Result<zeck_core::AddressInfo, String> {
    validate_destination_address(&address).map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn start_scan(
    app: AppHandle,
    state: State<'_, AppState>,
    config: ScanConfigInput,
) -> Result<ScanHandle, String> {
    let handle = state
        .service
        .start_scan(
            ScanConfig {
                birthday: config.birthday,
                num_accounts: config.num_accounts,
                gap_limit: config.gap_limit,
                lightwalletd_url: config.lightwalletd_url,
                network: config.network,
            },
            SecretString::new(config.seed),
        )
        .await
        .map_err(|err| err.to_string())?;

    let pump_service = state.service.clone();
    let pump_handle = handle.clone();
    tokio::spawn(async move {
        loop {
            let progress = match pump_service.get_scan_progress(&pump_handle).await {
                Ok(progress) => progress,
                Err(_) => break,
            };

            let _ = app.emit("scan-progress", &progress);

            if matches!(
                progress.phase,
                zeck_core::ScanPhase::Complete
                    | zeck_core::ScanPhase::Cancelled
                    | zeck_core::ScanPhase::Error
            ) {
                let _ = app.emit("scan-complete", &progress);
                break;
            }

            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    });

    Ok(handle)
}

#[tauri::command]
pub async fn get_scan_progress(
    state: State<'_, AppState>,
    handle: ScanHandle,
) -> Result<zeck_core::ScanProgress, String> {
    state
        .service
        .get_scan_progress(&handle)
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn cancel_scan(state: State<'_, AppState>, handle: ScanHandle) -> Result<(), String> {
    state
        .service
        .cancel_scan(&handle)
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn propose_sweep(
    state: State<'_, AppState>,
    handle: ScanHandle,
    destination: String,
) -> Result<SweepProposal, String> {
    state
        .service
        .propose_sweep(&handle, &destination)
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn execute_sweep(
    state: State<'_, AppState>,
    handle: ScanHandle,
    destination: String,
) -> Result<Vec<TxBroadcastResult>, String> {
    state
        .service
        .execute_sweep(&handle, &destination)
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn estimate_birthday_from_date(date: String) -> Result<u32, String> {
    estimate_birthday(&date).map_err(|err| err.to_string())
}
