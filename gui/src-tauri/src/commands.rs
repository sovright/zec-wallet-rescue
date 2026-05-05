use std::{fs, path::PathBuf};
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use std::process::Command;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use zeck_core::{
    detect_birthday as zeck_detect_birthday, estimate_birthday_from_date as estimate_birthday,
    validate_destination_address, validate_mnemonic_words, BirthdayDetectResult, MultiScanHandle,
    MultiSeedConfig, MultiSeedPhase, MultiSeedProgress, RecoveryService, ScanConfig, ScanHandle,
    SeedEntry, SweepProposal, SweepRequest, TxBroadcastResult, ZeckNetwork,
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
        // Track how many entries of the append-only discovery log we've
        // already forwarded — emit only the new tail each tick so the
        // frontend gets one event per discovery, never duplicates.
        let mut emitted_discoveries = 0usize;
        loop {
            let progress = match pump_service.get_scan_progress(&pump_handle).await {
                Ok(progress) => progress,
                Err(_) => break,
            };

            // Self-heal: the discovery log is contractually append-only,
            // but if a future bug ever shrinks it, clamp the cursor so
            // we don't index past the end and don't silently skip later
            // events.
            if emitted_discoveries > progress.discoveries.len() {
                emitted_discoveries = progress.discoveries.len();
            }
            if progress.discoveries.len() > emitted_discoveries {
                for discovery in &progress.discoveries[emitted_discoveries..] {
                    let _ = app.emit("scan-discovery", discovery);
                }
                emitted_discoveries = progress.discoveries.len();
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

#[derive(Debug, Clone, Deserialize)]
pub struct MultiSeedEntryInput {
    pub phrase: String,
    pub birthday: Option<u32>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MultiScanConfigInput {
    pub network: ZeckNetwork,
    pub lightwalletd_url: String,
    pub data_dir: String,
    pub gap_limit: u32,
    pub num_accounts: Option<u32>,
}

#[tauri::command]
pub async fn start_multi_scan(
    app: AppHandle,
    state: State<'_, AppState>,
    seeds: Vec<MultiSeedEntryInput>,
    config: MultiScanConfigInput,
) -> Result<MultiScanHandle, String> {
    let entries: Vec<SeedEntry> = seeds
        .into_iter()
        .map(|dto| SeedEntry {
            phrase: SecretString::new(dto.phrase),
            birthday: dto.birthday,
            label: dto.label,
        })
        .collect();

    let cfg = MultiSeedConfig {
        network: config.network,
        lightwalletd_url: config.lightwalletd_url,
        data_dir: PathBuf::from(config.data_dir),
        gap_limit: config.gap_limit,
        num_accounts: config.num_accounts,
    };

    let handle = state
        .service
        .start_multi_scan(entries, cfg)
        .await
        .map_err(|err| err.to_string())?;

    spawn_multi_scan_pump(state.service.clone(), handle, app);
    Ok(handle)
}

#[tauri::command]
pub async fn get_multi_scan_progress(
    state: State<'_, AppState>,
    handle: MultiScanHandle,
) -> Result<MultiSeedProgress, String> {
    state
        .service
        .get_multi_scan_progress(&handle)
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn cancel_multi_scan(
    state: State<'_, AppState>,
    handle: MultiScanHandle,
) -> Result<(), String> {
    state
        .service
        .cancel_multi_scan(&handle)
        .await
        .map_err(|err| err.to_string())
}

/// Background pump that samples the multi-seed run snapshot every 250ms and
/// fans out events to the frontend. The 250ms cadence is per-pump-loop and is
/// independent of the orchestrator's 1s internal aggregation tick — we just
/// re-emit whatever snapshot the service exposes. Discoveries are forwarded as
/// an append-only delta so the frontend never sees duplicates.
fn spawn_multi_scan_pump(service: RecoveryService, handle: MultiScanHandle, app: AppHandle) {
    tokio::spawn(async move {
        let mut emitted_discoveries = 0usize;
        loop {
            let progress = match service.get_multi_scan_progress(&handle).await {
                Ok(progress) => progress,
                Err(_) => break,
            };

            // Self-heal in case the discovery log ever shrinks (it is
            // contractually append-only, but be defensive).
            if emitted_discoveries > progress.discoveries.len() {
                emitted_discoveries = progress.discoveries.len();
            }
            if progress.discoveries.len() > emitted_discoveries {
                for discovery in &progress.discoveries[emitted_discoveries..] {
                    let _ = app.emit("multi-scan-discovery", discovery);
                }
                emitted_discoveries = progress.discoveries.len();
            }

            let _ = app.emit("multi-scan-progress", &progress);

            if matches!(
                progress.phase,
                MultiSeedPhase::Completed | MultiSeedPhase::Cancelled | MultiSeedPhase::Failed(_)
            ) {
                let _ = app.emit("multi-scan-complete", &progress);
                break;
            }

            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    });
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

/// Per-seed proposal returned by [`propose_sweep_all`]. `error` is set when
/// the per-seed proposal failed (e.g. unfunded seed → no spendable accounts);
/// `proposal` is set on success.
#[derive(Debug, Clone, Serialize)]
pub struct PerSeedSweepProposalDto {
    pub seed_index: usize,
    pub fingerprint: String,
    pub label: Option<String>,
    pub proposal: Option<SweepProposal>,
    pub error: Option<String>,
}

/// Per-seed execution result returned by [`execute_sweep_all`].
#[derive(Debug, Clone, Serialize)]
pub struct PerSeedSweepResultDto {
    pub seed_index: usize,
    pub fingerprint: String,
    pub label: Option<String>,
    pub txs: Vec<TxBroadcastResult>,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn propose_sweep_all(
    app: AppHandle,
    state: State<'_, AppState>,
    handle: MultiScanHandle,
    destination: String,
    memo: Option<String>,
    max_fee_zec: Option<String>,
) -> Result<Vec<PerSeedSweepProposalDto>, String> {
    let max_fee_zatoshis = max_fee_zec
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(parse_zec_to_zatoshis)
        .transpose()?;

    // Snapshot the run's progress so we know which seeds are present (and which
    // have a balance worth sweeping). Sweep contexts are stored on the
    // MultiSeedRun itself.
    let progress = state
        .service
        .get_multi_scan_progress(&handle)
        .await
        .map_err(|err| err.to_string())?;

    let mut results = Vec::with_capacity(progress.per_seed.len());
    for seed in &progress.per_seed {
        let dto = match state
            .service
            .propose_sweep_for_seed(
                &handle,
                seed.seed_index,
                SweepRequest {
                    destination: destination.clone(),
                    memo: memo.clone(),
                    max_fee_zatoshis,
                },
            )
            .await
        {
            Ok(proposal) => PerSeedSweepProposalDto {
                seed_index: seed.seed_index,
                fingerprint: seed.seed_fingerprint.clone(),
                label: seed.label.clone(),
                proposal: Some(proposal),
                error: None,
            },
            Err(err) => PerSeedSweepProposalDto {
                seed_index: seed.seed_index,
                fingerprint: seed.seed_fingerprint.clone(),
                label: seed.label.clone(),
                proposal: None,
                error: Some(err.to_string()),
            },
        };
        let _ = app.emit("multi-sweep-proposal-progress", &dto);
        results.push(dto);
    }
    Ok(results)
}

#[tauri::command]
pub async fn execute_sweep_all(
    app: AppHandle,
    state: State<'_, AppState>,
    handle: MultiScanHandle,
    destination: String,
    memo: Option<String>,
    max_fee_zec: Option<String>,
    seed_indexes: Option<Vec<usize>>,
) -> Result<Vec<PerSeedSweepResultDto>, String> {
    let max_fee_zatoshis = max_fee_zec
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(parse_zec_to_zatoshis)
        .transpose()?;

    let progress = state
        .service
        .get_multi_scan_progress(&handle)
        .await
        .map_err(|err| err.to_string())?;

    let targets: Vec<_> = match seed_indexes {
        Some(idxs) => progress
            .per_seed
            .iter()
            .filter(|s| idxs.contains(&s.seed_index))
            .cloned()
            .collect(),
        None => progress.per_seed.clone(),
    };

    let mut results = Vec::with_capacity(targets.len());
    for seed in &targets {
        let dto = match state
            .service
            .execute_sweep_for_seed(
                &handle,
                seed.seed_index,
                SweepRequest {
                    destination: destination.clone(),
                    memo: memo.clone(),
                    max_fee_zatoshis,
                },
            )
            .await
        {
            Ok(txs) => PerSeedSweepResultDto {
                seed_index: seed.seed_index,
                fingerprint: seed.seed_fingerprint.clone(),
                label: seed.label.clone(),
                txs,
                error: None,
            },
            Err(err) => PerSeedSweepResultDto {
                seed_index: seed.seed_index,
                fingerprint: seed.seed_fingerprint.clone(),
                label: seed.label.clone(),
                txs: Vec::new(),
                error: Some(err.to_string()),
            },
        };
        let _ = app.emit("multi-sweep-execution-progress", &dto);
        results.push(dto);
    }
    Ok(results)
}

#[tauri::command]
pub fn default_data_dir(app: AppHandle) -> Result<String, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("resolving app data dir: {err}"))?;
    Ok(base.join("workspace").display().to_string())
}

#[tauri::command]
pub async fn estimate_birthday_from_date(date: String) -> Result<u32, String> {
    estimate_birthday(&date).map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn detect_birthday(
    app: AppHandle,
    seed: String,
    lightwalletd_url: String,
    network: ZeckNetwork,
) -> Result<BirthdayDetectResult, String> {
    let seed_phrase = SecretString::new(seed);
    let app_clone = app.clone();
    zeck_detect_birthday(
        &seed_phrase,
        network,
        &lightwalletd_url,
        move |msg: &str| {
            let _ = app_clone.emit("birthday-probe-progress", msg.to_owned());
        },
    )
    .await
    .map_err(|err| err.to_string())
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

/// Best-effort OS-level notification used when a long scan finishes. Mirrors
/// the CLI implementation: shells out to platform tools so we don't pull in a
/// new Tauri plugin or Rust dependency. Errors are swallowed because the
/// notification is convenience, not a guarantee.
#[tauri::command]
pub async fn notify_user(title: String, body: String) -> Result<(), String> {
    if title.trim().is_empty() {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification {body} with title {title}",
            title = applescript_quote(&title),
            body = applescript_quote(&body),
        );
        let _ = Command::new("osascript").arg("-e").arg(script).status();
    }

    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("notify-send").arg(&title).arg(&body).status();
    }

    #[cfg(target_os = "windows")]
    {
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms;\
             $n=[System.Windows.Forms.NotifyIcon]::new();\
             $n.Icon=[System.Drawing.SystemIcons]::Information;\
             $n.Visible=$true;\
             $n.ShowBalloonTip(5000,{title},{body},0);\
             Start-Sleep 2;\
             $n.Dispose()",
            title = powershell_quote(&title),
            body = powershell_quote(&body),
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status();
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (title, body);
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn powershell_quote(input: &str) -> String {
    let escaped: String = input
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c == '\'' { "''".to_string() } else { c.to_string() })
        .collect();
    format!("'{escaped}'")
}

#[cfg(target_os = "macos")]
fn applescript_quote(input: &str) -> String {
    let escaped: String = input
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            '\\' => "\\\\".to_string(),
            '"' => "\\\"".to_string(),
            other => other.to_string(),
        })
        .collect();
    format!("\"{escaped}\"")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_zatoshi() {
        assert_eq!(parse_zec_to_zatoshis("0.00000001").unwrap(), 1);
    }

    #[test]
    fn whole_zec() {
        assert_eq!(parse_zec_to_zatoshis("1").unwrap(), 100_000_000);
    }

    #[test]
    fn mixed() {
        assert_eq!(parse_zec_to_zatoshis("0.0002").unwrap(), 20_000);
    }

    #[test]
    fn leading_dot() {
        assert_eq!(parse_zec_to_zatoshis(".5").unwrap(), 50_000_000);
    }

    #[test]
    fn too_many_decimals_rejected() {
        assert!(parse_zec_to_zatoshis("0.999999999").is_err());
    }

    #[test]
    fn negative_rejected() {
        assert!(parse_zec_to_zatoshis("-0.001").is_err());
    }

    #[test]
    fn empty_rejected() {
        assert!(parse_zec_to_zatoshis("").is_err());
    }

    #[test]
    fn non_numeric_rejected() {
        assert!(parse_zec_to_zatoshis("abc").is_err());
    }

    #[test]
    fn overflow_rejected() {
        assert!(parse_zec_to_zatoshis("99999999999999999999").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_quote_simple() {
        assert_eq!(applescript_quote("hello"), "\"hello\"");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_quote_escapes_double_quote() {
        assert_eq!(applescript_quote("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_quote_escapes_backslash() {
        assert_eq!(applescript_quote("C:\\path"), "\"C:\\\\path\"");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_quote_strips_control_chars() {
        assert_eq!(applescript_quote("abc\x00def"), "\"abcdef\"");
    }
}
