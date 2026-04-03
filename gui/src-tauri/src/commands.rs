use std::{collections::HashMap, fs, path::PathBuf};

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use zeck_core::{
    estimate_birthday_from_date as estimate_birthday, validate_destination_address,
    validate_mnemonic_words, RecoveryService, ScanConfig, ScanHandle, SweepProposal, SweepRequest,
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
    pub data_dir: String,
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
                data_dir: PathBuf::from(config.data_dir),
                network: config.network,
            },
            SecretString::new(config.seed),
        )
        .await
        .map_err(|err| err.to_string())?;

    let pump_service = state.service.clone();
    let pump_handle = handle.clone();
    tokio::spawn(async move {
        let mut seen_totals = HashMap::<u32, u64>::new();
        loop {
            let progress = match pump_service.get_scan_progress(&pump_handle).await {
                Ok(progress) => progress,
                Err(_) => break,
            };

            for account in &progress.accounts {
                let previous = seen_totals.insert(account.account_index, account.total_zatoshis);
                if account.total_zatoshis > 0 && previous != Some(account.total_zatoshis) {
                    let _ = app.emit("account-discovered", account);
                }
            }
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
    memo: Option<String>,
    max_fee_zec: Option<String>,
) -> Result<SweepProposal, String> {
    let max_fee_zatoshis = max_fee_zec
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(parse_zec_to_zatoshis)
        .transpose()?;

    state
        .service
        .propose_sweep(
            &handle,
            SweepRequest {
                destination,
                memo,
                max_fee_zatoshis,
            },
        )
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn execute_sweep(
    app: AppHandle,
    state: State<'_, AppState>,
    handle: ScanHandle,
    destination: String,
    memo: Option<String>,
    max_fee_zec: Option<String>,
) -> Result<Vec<TxBroadcastResult>, String> {
    let max_fee_zatoshis = max_fee_zec
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(parse_zec_to_zatoshis)
        .transpose()?;

    let results = state
        .service
        .execute_sweep(
            &handle,
            SweepRequest {
                destination,
                memo,
                max_fee_zatoshis,
            },
        )
        .await
        .map_err(|err| err.to_string())?;

    for result in &results {
        let _ = app.emit("sweep-tx-broadcast", result);
        if result.status == "confirmed" {
            let _ = app.emit("sweep-tx-confirmed", result);
        }
    }

    Ok(results)
}

#[tauri::command]
pub async fn estimate_birthday_from_date(date: String) -> Result<u32, String> {
    estimate_birthday(&date).map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn save_recovery_report(path: String, report: String) -> Result<String, String> {
    let path = PathBuf::from(path.trim());
    if path.as_os_str().is_empty() {
        return Err("report path cannot be empty".to_owned());
    }

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|err| format!("creating {}: {err}", parent.display()))?;
    }
    fs::write(&path, report).map_err(|err| format!("writing {}: {err}", path.display()))?;
    Ok(path.display().to_string())
}

fn parse_zec_to_zatoshis(input: &str) -> Result<u64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("max fee cannot be empty".to_owned());
    }

    let (whole, fractional) = match trimmed.split_once('.') {
        Some((whole, fractional)) => (whole, fractional),
        None => (trimmed, ""),
    };

    if fractional.len() > 8 {
        return Err("max fee supports at most 8 decimal places".to_owned());
    }

    let whole_part = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<u64>()
            .map_err(|_| "invalid whole ZEC amount".to_owned())?
    };

    let fractional_digits = if fractional.is_empty() {
        0
    } else {
        fractional
            .parse::<u64>()
            .map_err(|_| "invalid fractional ZEC amount".to_owned())?
    };

    let scale = 10u64.pow((8usize.saturating_sub(fractional.len())) as u32);
    whole_part
        .checked_mul(100_000_000)
        .and_then(|whole_zats| whole_zats.checked_add(fractional_digits.checked_mul(scale)?))
        .ok_or_else(|| "max fee is too large".to_owned())
}
