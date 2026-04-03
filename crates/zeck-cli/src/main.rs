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
#[command(name = "zeck", about = "Legacy ZecWallet Lite recovery tool")]
struct Cli {
    #[arg(long, conflicts_with = "seed_file")]
    seed: Option<String>,

    #[arg(long)]
    seed_file: Option<PathBuf>,

    #[arg(long, default_value = "./zeck_data")]
    data_dir: PathBuf,

    #[arg(
        long,
        visible_alias = "server",
        help = "lightwalletd gRPC endpoint(s); comma-separated URLs are tried in order",
        default_value = "https://mainnet.lightwalletd.com:9067"
    )]
    lightwalletd_url: String,

    #[arg(long)]
    num_accounts: Option<u32>,

    #[arg(long, default_value_t = 20)]
    gap_limit: u32,

    #[arg(long, default_value_t = 419_200)]
    birthday: u32,

    #[arg(long)]
    birthday_date: Option<String>,

    #[arg(long, value_enum, default_value_t = NetworkArg::Mainnet)]
    network: NetworkArg,

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
    ShowKeys,
    Scan,
    Sweep {
        #[arg(long)]
        destination: String,

        #[arg(long)]
        memo: Option<String>,

        #[arg(long, value_parser = parse_zec_to_zatoshis)]
        max_fee: Option<u64>,

        #[arg(long, conflicts_with = "confirm_sweep")]
        dry_run: bool,

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
                println!("Account {}", account.index);
                println!("  Sapling path: {}", account.sapling_path);
                println!("  Sapling address: {}", account.sapling_address);
                println!("  Unified address: {}", account.unified_address);
                println!(
                    "  Transparent receive: {}",
                    account.transparent_receive_address
                );
                println!(
                    "  Transparent change: {}",
                    account.transparent_change_address
                );
                println!(
                    "  Transparent receive path: {}",
                    account.transparent_receive_path
                );
                println!(
                    "  Transparent change path: {}",
                    account.transparent_change_path
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
            dry_run: _dry_run,
            confirm_sweep,
        } => {
            let address = validate_destination_address(&destination)?;
            println!(
                "Destination accepted as Unified Address: orchard={}, sapling={}, transparent={}",
                address.has_orchard, address.has_sapling, address.has_transparent
            );

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
                destination,
                memo,
                max_fee_zatoshis: max_fee,
            };
            let proposal = service.propose_sweep(&handle, request.clone()).await?;
            print_sweep_preview(&proposal);

            if confirm_sweep {
                let execution = service.execute_sweep(&handle, request).await;
                match execution {
                    Ok(results) => {
                        for result in results {
                            println!(
                                "account {}: {} {}",
                                result.source_account, result.status, result.detail
                            );
                        }
                    }
                    Err(err) => {
                        eprintln!("Sweep failed: {err}");
                        std::process::exit(1);
                    }
                }
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

async fn wait_for_scan(
    service: &RecoveryService,
    handle: &ScanHandle,
) -> Result<zeck_core::ScanProgress> {
    let spinner = ProgressBar::new_spinner();
    spinner
        .set_style(ProgressStyle::with_template("{spinner:.green} {msg}")?.tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "));
    spinner.enable_steady_tick(Duration::from_millis(120));

    loop {
        let progress = service.get_scan_progress(handle).await?;
        let phase = format!("{:?}", progress.phase);
        let message = progress.message.clone().unwrap_or_else(|| phase.clone());
        let progress_counts = if progress.blocks_total > 0 {
            format!(" [{} / {}]", progress.blocks_scanned, progress.blocks_total)
        } else {
            String::new()
        };
        let eta = progress
            .estimated_remaining_seconds
            .map(format_duration)
            .map(|eta| format!(" [ETA {eta}]"))
            .unwrap_or_default();
        spinner.set_message(format!("{phase}{progress_counts}{eta} {message}"));

        match progress.phase {
            phase if phase.is_terminal() => {
                spinner.finish_and_clear();
                return Ok(progress);
            }
            _ => tokio::time::sleep(Duration::from_millis(150)).await,
        }
    }
}

fn print_scan_result(progress: &zeck_core::ScanProgress) {
    println!("Phase: {:?}", progress.phase);

    if let Some(error) = &progress.error {
        println!("Error: {error}");
    }

    if let Some(message) = &progress.message {
        println!("Message: {message}");
    }

    if let Some(server) = &progress.server {
        println!(
            "lightwalletd: {} tip={} vendor={}",
            server.endpoint,
            server.latest_block_height.unwrap_or_default(),
            server.vendor.as_deref().unwrap_or("unknown")
        );
    }

    if let Some(summary) = &progress.summary {
        println!("Authoritative balances: {}", summary.authoritative_balances);
        println!("Workspace: {}", summary.workspace_dir);
        println!("{}", summary.note);
    }

    if progress.accounts.is_empty() {
        println!("No accounts derived.");
        return;
    }

    println!();
    for account in &progress.accounts {
        println!(
            "Account {}  total={} zats  transparent={} zats",
            account.account_index, account.total_zatoshis, account.transparent_zatoshis
        );
        println!("  Status: {}", account.status);
        println!("  Sapling: {}", account.sapling_address);
        println!("  Unified: {}", account.unified_address);
        println!(
            "  Transparent receive: {}",
            account.transparent_receive_address
        );
        println!(
            "  Transparent change: {}",
            account.transparent_change_address
        );
    }
}

fn print_sweep_preview(proposal: &SweepProposal) {
    println!();
    println!("Sweep preview");
    println!("  total send: {} zats", proposal.total_send_zatoshis);
    println!("  total fee: {} zats", proposal.total_fee_zatoshis);
    println!("  net receive: {} zats", proposal.net_received_zatoshis);
    if !proposal.transactions.is_empty() {
        println!("  transactions:");
        for tx in &proposal.transactions {
            let memo = tx.memo.as_deref().unwrap_or("-");
            println!(
                "    account {} {:?} gross={} fee={} net={} -> {} memo={}",
                tx.source_account,
                tx.kind,
                tx.gross_zatoshis,
                tx.fee_zatoshis,
                tx.net_zatoshis,
                tx.destination,
                memo
            );
        }
    }
    if !proposal.skipped_accounts.is_empty() {
        println!("  skipped accounts:");
        for skipped in &proposal.skipped_accounts {
            println!(
                "    account {} gross={} reason={}",
                skipped.account_index, skipped.gross_zatoshis, skipped.reason
            );
        }
    }
    if let Some(warning) = &proposal.warning {
        println!("  warning: {warning}");
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
