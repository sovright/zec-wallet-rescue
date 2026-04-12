use std::{fs, path::PathBuf, time::Duration};

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

        let phase = format!("{:?}", progress.phase);
        let server_label = progress
            .server
            .as_ref()
            .map(|s| format!(" [{}]", s.endpoint))
            .unwrap_or_default();
        let eta = progress
            .estimated_remaining_seconds
            .map(format_duration)
            .map(|t| format!(" ETA {t}"))
            .unwrap_or_default();

        let msg = format!("{phase}{server_label}{eta}");

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

fn format_duration(seconds: u64) -> String {
    let minutes = seconds / 60;
    let remaining_seconds = seconds % 60;
    if minutes == 0 {
        format!("{remaining_seconds}s")
    } else {
        format!("{minutes}m {remaining_seconds:02}s")
    }
}