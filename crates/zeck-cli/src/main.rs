use std::{collections::VecDeque, fs, io::Write, path::PathBuf, time::{Duration, Instant}};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use dialoguer::Password;
use indicatif::{ProgressBar, ProgressStyle};
use secrecy::SecretString;
use tracing_subscriber::EnvFilter;
use zeck_core::{
    derive_accounts, estimate_birthday_from_date, validate_destination_address, RecoveryService,
    ScanConfig, ScanHandle, ScanPhase, SweepProposal, SweepRequest, ZeckNetwork,
};

#[derive(Debug, Parser)]
#[command(
    name = "zeck",
    about = "Legacy ZecWallet Lite recovery tool",
    long_about = "ZECK recovers funds from ZecWallet Lite wallets using a BIP-39 seed phrase.\n\
                  It derives keys, scans the Zcash blockchain via lightwalletd, and can sweep\n\
                  recovered funds to a new Unified Address.",
    version
)]
struct Cli {
    /// BIP-39 seed phrase (24 words). Omit to be prompted securely.
    #[arg(long, conflicts_with = "seed_file")]
    seed: Option<String>,

    /// Path to a plain-text file containing the 24-word seed phrase.
    #[arg(long)]
    seed_file: Option<PathBuf>,

    /// Directory for wallet database and block cache.
    #[arg(long, default_value = "./zeck_data")]
    data_dir: PathBuf,

    /// lightwalletd gRPC endpoint(s). Comma-separated URLs are tried in order.
    #[arg(
        long,
        visible_alias = "server",
        default_value = "https://mainnet.lightwalletd.com:9067"
    )]
    lightwalletd_url: String,

    /// Scan exactly this many accounts (overrides --gap-limit).
    #[arg(long)]
    num_accounts: Option<u32>,

    /// Stop after this many consecutive empty accounts (ignored when --num-accounts is set).
    #[arg(long, default_value_t = 20)]
    gap_limit: u32,

    /// Wallet birthday as a block height. Use 0 for a full scan from genesis.
    #[arg(long, default_value_t = 419_200)]
    birthday: u32,

    /// Wallet creation date (YYYY-MM-DD). Estimates birthday height automatically.
    #[arg(long)]
    birthday_date: Option<String>,

    /// Zcash network to use.
    #[arg(long, value_enum, default_value_t = NetworkArg::Mainnet)]
    network: NetworkArg,

    /// Enable debug-level logging from zeck-core.
    #[arg(long)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum NetworkArg {
    Mainnet,
    Testnet,
}

impl From<NetworkArg> for ZeckNetwork {
    fn from(value: NetworkArg) -> Self {
        match value {
            NetworkArg::Mainnet => ZeckNetwork::Mainnet,
            NetworkArg::Testnet => ZeckNetwork::Testnet,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Derive and display all account keys and addresses (no network needed).
    ShowKeys,

    /// Scan the blockchain and report balances for all derived accounts.
    Scan,

    /// Scan and then sweep recovered funds to a Unified Address.
    Sweep {
        /// Destination Unified Address (must include Orchard or Sapling receiver).
        #[arg(long)]
        destination: String,

        /// Optional memo attached to shielded outputs (max 512 bytes).
        #[arg(long)]
        memo: Option<String>,

        /// Maximum fee in ZEC (e.g. 0.001). Sweep is skipped if estimated fee exceeds this.
        #[arg(long, value_parser = parse_zec_to_zatoshis)]
        max_fee: Option<u64>,

        /// Preview the sweep proposal without broadcasting any transactions.
        #[arg(long, conflicts_with = "confirm_sweep")]
        dry_run: bool,

        /// Confirm you understand this is irreversible and broadcast the sweep.
        #[arg(long)]
        confirm_sweep: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose)?;

    let network: ZeckNetwork = cli.network.into();
    let birthday = if let Some(date) = &cli.birthday_date {
        estimate_birthday_from_date(date)?
    } else {
        cli.birthday
    };

    let seed_phrase = load_seed_phrase(cli.seed, cli.seed_file)?;
    let account_count = cli.num_accounts.unwrap_or(20);

    if matches!(cli.command, Commands::Scan | Commands::Sweep { .. }) {
        eprintln!(
            "Note: this scan can take hours for old wallets. Progress is saved \
             under {data_dir} after each batch — interrupt with Ctrl-C any time \
             and re-run with the same flags (network, birthday, gap-limit) to \
             resume from the last persisted block. Changing any of those flags \
             intentionally starts a fresh workspace and re-scans from the new \
             birthday.",
            data_dir = cli.data_dir.display(),
        );
    }

    match cli.command {
        Commands::ShowKeys => {
            let accounts = derive_accounts(&seed_phrase, network, account_count)?;
            for account in accounts {
                println!("━━━ Account {} ━━━", account.index);
                println!("  Unified address     {}", account.unified_address);
                println!("  Sapling address     {}", account.sapling_address);
                println!("  Sapling path        {}", account.sapling_path);
                println!(
                    "  Transparent receive {}  ({})",
                    account.transparent_receive_address, account.transparent_receive_path
                );
                println!(
                    "  Transparent change  {}  ({})",
                    account.transparent_change_address, account.transparent_change_path
                );
                println!();
            }
        }

        Commands::Scan => {
            let service = RecoveryService::new();
            let handle = service
                .start_scan(
                    ScanConfig {
                        birthday,
                        num_accounts: cli.num_accounts,
                        gap_limit: cli.gap_limit,
                        lightwalletd_url: cli.lightwalletd_url,
                        data_dir: cli.data_dir.clone(),
                        network,
                    },
                    seed_phrase,
                )
                .await?;

            let progress = wait_for_scan(&service, &handle).await?;
            print_scan_result(&progress);
            notify_scan_complete(&progress);
            if progress.phase == ScanPhase::Cancelled {
                std::process::exit(130);
            }
            if progress.phase == ScanPhase::Error {
                bail!("recovery scan failed");
            }
        }

        Commands::Sweep {
            destination,
            memo,
            max_fee,
            dry_run,
            confirm_sweep,
        } => {
            let address = validate_destination_address(&destination)?;
            println!(
                "Destination: Unified Address (Orchard={}, Sapling={}, Transparent={})",
                address.has_orchard, address.has_sapling, address.has_transparent
            );

            if dry_run {
                println!();
                println!("╔══════════════════════════════════════╗");
                println!("║  DRY RUN — no funds will be moved    ║");
                println!("╚══════════════════════════════════════╝");
                println!();
            }

            let service = RecoveryService::new();
            let handle = service
                .start_scan(
                    ScanConfig {
                        birthday,
                        num_accounts: cli.num_accounts,
                        gap_limit: cli.gap_limit,
                        lightwalletd_url: cli.lightwalletd_url,
                        data_dir: cli.data_dir.clone(),
                        network,
                    },
                    seed_phrase,
                )
                .await?;

            let progress = wait_for_scan(&service, &handle).await?;
            print_scan_result(&progress);
            notify_scan_complete(&progress);
            if progress.phase == ScanPhase::Cancelled {
                std::process::exit(130);
            }
            if progress.phase == ScanPhase::Error {
                bail!("recovery scan failed");
            }

            let request = SweepRequest {
                destination: destination.clone(),
                memo: memo.clone(),
                max_fee_zatoshis: max_fee,
            };
            let proposal = service.propose_sweep(&handle, request.clone()).await?;
            print_sweep_preview(&proposal);

            if dry_run {
                println!();
                println!("Dry run complete. Re-run with --confirm-sweep to broadcast.");
                return Ok(());
            }

            if confirm_sweep {
                println!();
                println!("Broadcasting sweep transactions…");
                let execution = service.execute_sweep(&handle, request).await;
                match execution {
                    Ok(results) => {
                        println!();
                        for result in &results {
                            println!(
                                "  account {}  {}  {}",
                                result.source_account, result.status, result.detail
                            );
                        }
                        println!();
                        println!("Sweep complete.");
                    }
                    Err(err) => {
                        eprintln!();
                        eprintln!("Sweep failed: {err}");
                        std::process::exit(1);
                    }
                }
            } else {
                println!();
                println!("Re-run with --dry-run to preview, or --confirm-sweep to broadcast.");
            }
        }
    }

    Ok(())
}

fn init_tracing(verbose: bool) -> Result<()> {
    let filter = if verbose {
        EnvFilter::new("zeck_core=debug,zeck_cli=debug")
    } else {
        EnvFilter::new("warn")
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    Ok(())
}

fn load_seed_phrase(seed: Option<String>, seed_file: Option<PathBuf>) -> Result<SecretString> {
    if let Some(seed) = seed {
        return Ok(SecretString::new(seed.trim().to_owned()));
    }

    if let Some(path) = seed_file {
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read seed file {}", path.display()))?;
        return Ok(SecretString::new(contents.trim().to_owned()));
    }

    let phrase = Password::new()
        .with_prompt("Enter your 24-word seed phrase")
        .allow_empty_password(false)
        .interact()
        .context("failed to read seed phrase from terminal")?;

    Ok(SecretString::new(phrase.trim().to_owned()))
}

/// Parse a ZEC string (e.g. "0.001") into zatoshis.
fn parse_zec_to_zatoshis(input: &str) -> Result<u64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("max fee cannot be empty".to_owned());
    }

    let (whole, fractional) = match trimmed.split_once('.') {
        Some((whole, frac)) => (whole, frac),
        None => (trimmed, ""),
    };

    if fractional.len() > 8 {
        return Err("max fee supports at most 8 decimal places".to_owned());
    }

    let whole_part = if whole.is_empty() {
        0u64
    } else {
        whole
            .parse::<u64>()
            .map_err(|_| "invalid whole ZEC amount".to_owned())?
    };

    let fractional_digits = if fractional.is_empty() {
        0u64
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

/// Format zatoshis as a human-readable ZEC amount (e.g. "1.23456789 ZEC").
fn format_zec(zatoshis: u64) -> String {
    let whole = zatoshis / 100_000_000;
    let frac = zatoshis % 100_000_000;
    if frac == 0 {
        format!("{whole} ZEC")
    } else {
        format!("{whole}.{frac:08} ZEC")
    }
}

async fn wait_for_scan(
    service: &RecoveryService,
    handle: &ScanHandle,
) -> Result<zeck_core::ScanProgress> {
    // Start with a spinner; upgrade to a real progress bar once we know total blocks.
    let bar = ProgressBar::new_spinner();
    bar.set_style(
        ProgressStyle::with_template("{spinner:.green} {msg}")?
            .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "),
    );
    bar.enable_steady_tick(Duration::from_millis(120));

    let mut bar_has_total = false;
    let mut eta = EtaTracker::new();
    let started_at = Instant::now();

    loop {
        let progress = service.get_scan_progress(handle).await?;

        // Upgrade spinner → progress bar the first time we have block counts.
        if !bar_has_total && progress.blocks_total > 0 {
            bar.set_length(progress.blocks_total);
            bar.set_style(
                ProgressStyle::with_template(
                    "{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} blocks  {msg}",
                )?
                .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ")
                .progress_chars("█▉▊▋▌▍▎▏  "),
            );
            bar_has_total = true;
        }

        eta.observe(progress.blocks_scanned, progress.blocks_total);

        let phase_label = phase_label(&progress);
        let server_label = progress
            .server
            .as_ref()
            .map(|s| format!(" [{}]", s.endpoint))
            .unwrap_or_default();
        let eta_label = match eta.estimate(started_at.elapsed()) {
            EtaEstimate::Warmup => " · Estimating remaining time…".to_string(),
            EtaEstimate::Range(text) => format!(" · {text}"),
            EtaEstimate::Done => String::new(),
        };
        let era_label = era_hint(progress.blocks_scanned)
            .map(|era| format!(" · scanning ~{era}"))
            .unwrap_or_default();

        let msg = format!("{phase_label}{server_label}{era_label}{eta_label}");

        if bar_has_total {
            bar.set_position(progress.blocks_scanned);
        }
        bar.set_message(msg);

        if progress.phase.is_terminal() {
            bar.finish_and_clear();
            return Ok(progress);
        }

        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

fn phase_label(progress: &zeck_core::ScanProgress) -> String {
    match progress.phase {
        ScanPhase::Idle => "Starting".to_string(),
        ScanPhase::ValidatingSeed => "Validating seed".to_string(),
        ScanPhase::DerivingKeys => "Deriving keys".to_string(),
        ScanPhase::ProbingLightwalletd => "Connecting to lightwalletd".to_string(),
        ScanPhase::ScanningTransparent => "Scanning transparent addresses".to_string(),
        ScanPhase::ScanningShielded => "Decrypting shielded transactions".to_string(),
        ScanPhase::Complete => "Complete".to_string(),
        ScanPhase::Cancelled => "Cancelled".to_string(),
        ScanPhase::Error => "Error".to_string(),
    }
}

/// Sliding-window ETA tracker that ignores the noisy first few seconds and
/// returns a rounded range rather than a false-precision point estimate.
struct EtaTracker {
    samples: VecDeque<(Instant, u64)>,
    last_total: u64,
}

enum EtaEstimate {
    /// Not enough data yet — show a "Estimating…" message.
    Warmup,
    /// Stable estimate, formatted human-readably.
    Range(String),
    /// Either no work to do or already done.
    Done,
}

impl EtaTracker {
    const WARMUP: Duration = Duration::from_secs(15);
    const WINDOW: Duration = Duration::from_secs(45);

    fn new() -> Self {
        Self { samples: VecDeque::new(), last_total: 0 }
    }

    fn observe(&mut self, scanned: u64, total: u64) {
        if total == 0 {
            return;
        }
        self.last_total = total;
        let now = Instant::now();
        self.samples.push_back((now, scanned));
        let cutoff = now - Self::WINDOW;
        while let Some(&(t, _)) = self.samples.front() {
            if t < cutoff && self.samples.len() > 2 {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    fn estimate(&self, elapsed: Duration) -> EtaEstimate {
        let Some(&(t_first, blocks_first)) = self.samples.front() else {
            return EtaEstimate::Warmup;
        };
        let Some(&(t_last, blocks_last)) = self.samples.back() else {
            return EtaEstimate::Warmup;
        };
        if self.last_total == 0 {
            return EtaEstimate::Warmup;
        }

        let remaining = self.last_total.saturating_sub(blocks_last);
        if remaining == 0 {
            return EtaEstimate::Done;
        }

        let window = t_last.saturating_duration_since(t_first);
        let scanned_in_window = blocks_last.saturating_sub(blocks_first);
        if elapsed < Self::WARMUP || window < Duration::from_secs(5) || scanned_in_window < 50 {
            return EtaEstimate::Warmup;
        }

        let rate = scanned_in_window as f64 / window.as_secs_f64();
        if rate <= 0.0 {
            return EtaEstimate::Warmup;
        }

        let secs = (remaining as f64 / rate).round() as u64;
        EtaEstimate::Range(format_eta_range(secs))
    }
}

/// Returns a human-readable time range with rounding tuned to how uncertain we
/// expect each band to be. Falsifies precision deliberately — at 6h out, the
/// difference between 5h47m and 6h13m is meaningless to a waiting user.
fn format_eta_range(secs: u64) -> String {
    if secs < 60 {
        return "less than a minute remaining".to_string();
    }
    if secs < 5 * 60 {
        return "less than 5 minutes remaining".to_string();
    }
    if secs < 30 * 60 {
        let mins = ((secs as f64 / 60.0 / 5.0).round() as u64) * 5;
        return format!("about {mins} minutes remaining");
    }
    if secs < 60 * 60 {
        return "less than an hour remaining".to_string();
    }
    let hours = secs as f64 / 3600.0;
    if hours < 2.0 {
        return "about 1-2 hours remaining".to_string();
    }
    let lo = hours.floor() as u64;
    let hi = lo + 1;
    format!("about {lo}-{hi} hours remaining")
}

/// Map a block height to its approximate calendar year on mainnet so users can
/// feel the scan moving through time. Uses ~82 s/block long-run average from
/// Sapling activation (height 419,200, 2018-10-28).
fn era_hint(height: u64) -> Option<String> {
    if height == 0 {
        return None;
    }
    const SAPLING_HEIGHT: u64 = 419_200;
    const SAPLING_YEAR: i32 = 2018;
    const SECONDS_PER_BLOCK: f64 = 82.0;
    if height < SAPLING_HEIGHT {
        return Some("pre-Sapling era".to_string());
    }
    let elapsed_secs = (height - SAPLING_HEIGHT) as f64 * SECONDS_PER_BLOCK;
    let elapsed_years = elapsed_secs / (365.25 * 86_400.0);
    // Sapling activated late October — round forward so blocks shortly after
    // activation read as 2019, not 2018.
    let year = SAPLING_YEAR + (elapsed_years + 0.18) as i32;
    Some(year.to_string())
}

fn print_scan_result(progress: &zeck_core::ScanProgress) {
    println!("Phase: {:?}", progress.phase);

    if let Some(error) = &progress.error {
        eprintln!("Error: {error}");
    }

    if let Some(server) = &progress.server {
        println!(
            "lightwalletd: {}  tip={}  vendor={}",
            server.endpoint,
            server.latest_block_height.unwrap_or_default(),
            server.vendor.as_deref().unwrap_or("unknown")
        );
    }

    if let Some(summary) = &progress.summary {
        println!("Authoritative balances: {}", summary.authoritative_balances);
        println!("Workspace: {}", summary.workspace_dir);
        if !summary.note.is_empty() {
            println!("Note: {}", summary.note);
        }
    }

    if progress.accounts.is_empty() {
        println!("No accounts derived.");
        return;
    }

    println!();
    println!("{:<8}  {:>16}  {:>16}  {:>16}  Status", "Account", "Sapling", "Orchard", "Transparent");
    println!("{}", "─".repeat(80));
    for account in &progress.accounts {
        println!(
            "{:<8}  {:>16}  {:>16}  {:>16}  {}",
            account.account_index,
            format_zec(account.sapling_zatoshis),
            format_zec(account.orchard_zatoshis),
            format_zec(account.transparent_zatoshis),
            account.status,
        );
    }
    println!("{}", "─".repeat(80));
    let total: u64 = progress.accounts.iter().map(|a| a.total_zatoshis).sum();
    println!("{:<8}  {:>52}  Total: {}", "", "", format_zec(total));
    println!();
    for account in &progress.accounts {
        if account.total_zatoshis > 0 {
            println!("Account {}  addresses:", account.account_index);
            println!("  Unified:              {}", account.unified_address);
            println!("  Sapling:              {}", account.sapling_address);
            println!("  Transparent receive:  {}", account.transparent_receive_address);
            println!("  Transparent change:   {}", account.transparent_change_address);
            println!();
        }
    }
}

fn print_sweep_preview(proposal: &SweepProposal) {
    println!();
    println!("Sweep preview:");
    println!("  Send:        {}", format_zec(proposal.total_send_zatoshis));
    println!("  Fee:         {}", format_zec(proposal.total_fee_zatoshis));
    println!("  Net receive: {}", format_zec(proposal.net_received_zatoshis));

    if !proposal.transactions.is_empty() {
        println!();
        println!("  Transactions:");
        for tx in &proposal.transactions {
            let memo = tx.memo.as_deref().unwrap_or("—");
            println!(
                "    account {:>3}  {:?}  gross={}  fee={}  net={}  memo={}",
                tx.source_account,
                tx.kind,
                format_zec(tx.gross_zatoshis),
                format_zec(tx.fee_zatoshis),
                format_zec(tx.net_zatoshis),
                memo,
            );
        }
    }

    if !proposal.skipped_accounts.is_empty() {
        println!();
        println!("  Skipped accounts:");
        for skipped in &proposal.skipped_accounts {
            println!(
                "    account {:>3}  gross={}  reason={}",
                skipped.account_index,
                format_zec(skipped.gross_zatoshis),
                skipped.reason,
            );
        }
    }

    if let Some(warning) = &proposal.warning {
        println!();
        println!("  Warning: {warning}");
    }
}

/// Try to grab the user's attention when a long-running scan finishes. Best
/// effort: terminal bell always; OS-level notification on macOS/Linux when the
/// usual platform tools are present. Errors are silently swallowed because the
/// scan succeeded — failing to notify is not a scan failure.
fn notify_scan_complete(progress: &zeck_core::ScanProgress) {
    let title = match progress.phase {
        ScanPhase::Complete => "ZECK scan complete",
        ScanPhase::Cancelled => "ZECK scan cancelled",
        ScanPhase::Error => "ZECK scan failed",
        _ => return,
    };

    let body = scan_completion_summary(progress);

    // Terminal bell. ANSI BEL is ignored by quiet terminals but harmless.
    let _ = std::io::stderr().write_all(b"\x07");

    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification {body} with title {title}",
            title = applescript_quote(title),
            body = applescript_quote(&body),
        );
        let _ = Command::new("osascript").arg("-e").arg(script).status();
    }

    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("notify-send").arg(title).arg(&body).status();
    }

    // Windows toast notifications would need a separate dependency
    // (e.g. winrt-notification); intentionally deferred to keep this PR
    // dependency-free. The terminal bell still fires.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (title, body);
    }
}

fn scan_completion_summary(progress: &zeck_core::ScanProgress) -> String {
    if let Some(error) = &progress.error {
        return error.clone();
    }
    let funded: Vec<_> = progress
        .accounts
        .iter()
        .filter(|a| a.total_zatoshis > 0)
        .collect();
    let total: u64 = funded.iter().map(|a| a.total_zatoshis).sum();
    if funded.is_empty() {
        return "No funds were found across all scanned accounts.".to_string();
    }
    let zec = format_zec(total);
    match funded.len() {
        1 => format!("Found {zec} on 1 account."),
        n => format!("Found {zec} across {n} accounts."),
    }
}

#[cfg(target_os = "macos")]
fn applescript_quote(input: &str) -> String {
    // AppleScript string literal: wrap in double quotes, escape backslashes
    // and double quotes. Strip control chars to keep `osascript` happy.
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

    #[test]
    fn eta_under_a_minute_is_friendly() {
        assert_eq!(format_eta_range(45), "less than a minute remaining");
    }

    #[test]
    fn eta_under_five_minutes_is_friendly() {
        assert_eq!(format_eta_range(180), "less than 5 minutes remaining");
    }

    #[test]
    fn eta_minute_band_rounds_to_five() {
        // 7 minutes → "about 5 minutes remaining" (rounded down to nearest 5)
        assert_eq!(format_eta_range(7 * 60), "about 5 minutes remaining");
        // 13 minutes → "about 15 minutes remaining"
        assert_eq!(format_eta_range(13 * 60), "about 15 minutes remaining");
    }

    #[test]
    fn eta_under_an_hour_is_friendly() {
        assert_eq!(format_eta_range(45 * 60), "less than an hour remaining");
    }

    #[test]
    fn eta_short_hour_band_is_a_one_to_two() {
        assert_eq!(format_eta_range(80 * 60), "about 1-2 hours remaining");
    }

    #[test]
    fn eta_multi_hour_band_is_a_one_hour_window() {
        assert_eq!(format_eta_range(3 * 3600 + 1800), "about 3-4 hours remaining");
        assert_eq!(format_eta_range(7 * 3600), "about 7-8 hours remaining");
    }

    #[test]
    fn era_hint_for_genesis_is_pre_sapling() {
        assert_eq!(era_hint(100_000).as_deref(), Some("pre-Sapling era"));
    }

    #[test]
    fn era_hint_just_after_activation_is_2018() {
        assert_eq!(era_hint(420_000).as_deref(), Some("2018"));
    }

    #[test]
    fn era_hint_for_recent_height_is_recent_year() {
        // Block ~3.3M corresponds to ~2026.
        let era = era_hint(3_300_000).unwrap();
        assert!(
            era == "2025" || era == "2026",
            "expected 2025/2026 for height 3.3M, got {era}"
        );
    }

    #[test]
    fn era_hint_zero_is_none() {
        assert!(era_hint(0).is_none());
    }

    #[test]
    fn completion_summary_no_funds() {
        let progress = make_progress(ScanPhase::Complete, &[]);
        assert_eq!(
            scan_completion_summary(&progress),
            "No funds were found across all scanned accounts."
        );
    }

    #[test]
    fn completion_summary_one_account() {
        let progress = make_progress(ScanPhase::Complete, &[(0, 50_000_000)]);
        assert_eq!(
            scan_completion_summary(&progress),
            "Found 0.50000000 ZEC on 1 account."
        );
    }

    #[test]
    fn completion_summary_multiple_accounts() {
        let progress = make_progress(
            ScanPhase::Complete,
            &[(0, 100_000_000), (3, 50_000_000)],
        );
        assert_eq!(
            scan_completion_summary(&progress),
            "Found 1.50000000 ZEC across 2 accounts."
        );
    }

    #[test]
    fn completion_summary_uses_error_when_present() {
        let mut progress = make_progress(ScanPhase::Error, &[]);
        progress.error = Some("lightwalletd unreachable".to_string());
        assert_eq!(
            scan_completion_summary(&progress),
            "lightwalletd unreachable"
        );
    }

    fn make_progress(
        phase: ScanPhase,
        funded: &[(u32, u64)],
    ) -> zeck_core::ScanProgress {
        let accounts = funded
            .iter()
            .map(|(idx, amount)| zeck_core::AccountBalancePreview {
                account_index: *idx,
                sapling_address: String::new(),
                unified_address: String::new(),
                transparent_receive_address: String::new(),
                transparent_change_address: String::new(),
                transparent_utxo_count: 0,
                sapling_zatoshis: 0,
                orchard_zatoshis: *amount,
                transparent_zatoshis: 0,
                total_zatoshis: *amount,
                has_activity: true,
                status: String::new(),
            })
            .collect();
        zeck_core::ScanProgress {
            handle: zeck_core::ScanHandle::new(),
            phase,
            blocks_scanned: 0,
            blocks_total: 0,
            elapsed_seconds: None,
            estimated_remaining_seconds: None,
            accounts,
            summary: None,
            server: None,
            message: None,
            error: None,
        }
    }
}