use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use std::process::Command;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use zeck_core::{
    detect_birthday as zeck_detect_birthday, estimate_birthday_from_date as estimate_birthday,
    validate_destination_address, validate_mnemonic_words, BirthdayDetectResult, RecoveryService,
    ScanConfig, ScanHandle, SweepProposal, SweepRequest, TxBroadcastResult, ZeckNetwork,
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
pub fn default_data_dir(app: AppHandle) -> Result<String, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("resolving app data dir: {err}"))?;
    Ok(base.join("workspace").display().to_string())
}

#[tauri::command]
pub async fn estimate_birthday_from_date(
    date: String,
    lightwalletd_url: String,
) -> Result<u32, String> {
    estimate_birthday(&date, &lightwalletd_url)
        .await
        .map_err(|err| err.to_string())
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
pub async fn save_recovery_report(
    state: State<'_, AppState>,
    handle: ScanHandle,
    path: String,
    report: String,
) -> Result<String, String> {
    let progress = state
        .service
        .get_scan_progress(&handle)
        .await
        .map_err(|err| err.to_string())?;
    let workspace_dir = progress
        .summary
        .as_ref()
        .map(|summary| PathBuf::from(&summary.workspace_dir))
        .ok_or_else(|| "recovery report can only be saved after workspace sync".to_owned())?;
    let path = resolve_report_path(&workspace_dir, path.trim())?;

    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() {
            return Err("report path must not be a symlink".to_owned());
        }
        if !metadata.is_file() {
            return Err("report path must refer to a regular file".to_owned());
        }
    }

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .map_err(|err| format!("opening {}: {err}", path.display()))?;
    file.write_all(report.as_bytes())
        .map_err(|err| format!("writing {}: {err}", path.display()))?;
    Ok(path.display().to_string())
}

fn resolve_report_path(workspace_dir: &Path, requested: &str) -> Result<PathBuf, String> {
    if requested.is_empty() {
        return Err("report path cannot be empty".to_owned());
    }

    let requested = PathBuf::from(requested);
    if requested
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("report path must stay inside the recovery workspace".to_owned());
    }

    let workspace_root = fs::canonicalize(workspace_dir).map_err(|err| {
        format!(
            "opening recovery workspace {}: {err}",
            workspace_dir.display()
        )
    })?;
    let candidate = if requested.is_absolute() {
        requested
    } else {
        workspace_root.join(requested)
    };
    let parent = candidate
        .parent()
        .ok_or_else(|| "report path must include a file name".to_owned())?;

    // Auto-create relative subdirectories under the workspace, but never blindly
    // mkdir for absolute user-supplied paths — those must already exist so the
    // canonicalize-and-prefix-check below can guard against escapes.
    if !parent.exists() && parent.starts_with(&workspace_root) {
        fs::create_dir_all(parent)
            .map_err(|err| format!("creating {}: {err}", parent.display()))?;
    }

    let canonical_parent = fs::canonicalize(parent)
        .map_err(|err| format!("report directory must be inside the workspace: {err}"))?;
    if !canonical_parent.starts_with(&workspace_root) {
        return Err("report path must stay inside the recovery workspace".to_owned());
    }
    let file_name = candidate
        .file_name()
        .filter(|file_name| !file_name.is_empty())
        .ok_or_else(|| "report path must include a file name".to_owned())?;

    Ok(canonical_parent.join(file_name))
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_workspace() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "zeck-report-path-test-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp workspace should be created");
        path
    }

    #[test]
    fn report_path_resolves_inside_workspace() {
        let workspace = temp_workspace();
        let path = resolve_report_path(&workspace, "zeck-recovery-report.txt")
            .expect("report path should resolve");

        assert!(path.starts_with(fs::canonicalize(&workspace).expect("canonical workspace")));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("zeck-recovery-report.txt")
        );

        fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    }

    #[test]
    fn report_path_rejects_parent_traversal() {
        let workspace = temp_workspace();
        let err = resolve_report_path(&workspace, "../outside.txt")
            .expect_err("parent traversal should be rejected");

        assert!(err.contains("inside the recovery workspace"));

        fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    }

    #[test]
    fn report_path_auto_creates_subdir_inside_workspace() {
        let workspace = temp_workspace();
        let path = resolve_report_path(&workspace, "reports/zeck-recovery-report.txt")
            .expect("subdir should be auto-created");

        let canonical_workspace = fs::canonicalize(&workspace).expect("canonical workspace");
        assert!(path.starts_with(&canonical_workspace));
        assert!(path.parent().expect("parent").exists());

        fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    }

    #[test]
    fn report_path_accepts_absolute_path_inside_workspace() {
        let workspace = temp_workspace();
        let canonical_workspace = fs::canonicalize(&workspace).expect("canonical workspace");
        let absolute = canonical_workspace.join("zeck-recovery-report.txt");

        let path = resolve_report_path(
            &workspace,
            absolute.to_str().expect("absolute path should be utf-8"),
        )
        .expect("absolute path inside workspace should resolve");

        assert_eq!(path, absolute);

        fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    }

    #[test]
    fn report_path_rejects_absolute_path_outside_workspace() {
        let workspace = temp_workspace();
        let outside = std::env::temp_dir().join("zeck-outside-report.txt");

        let err = resolve_report_path(
            &workspace,
            outside.to_str().expect("outside path should be utf-8"),
        )
        .expect_err("absolute path outside workspace should be rejected");

        // Either the parent canonicalization fails (parent missing) or the
        // prefix check fails — both are acceptable rejections.
        assert!(
            err.contains("inside the recovery workspace")
                || err.contains("must be inside the workspace")
        );

        fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    }

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
