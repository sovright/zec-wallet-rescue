use std::{fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use dialoguer::Password;
use indicatif::{ProgressBar, ProgressStyle};
use secrecy::SecretString;
use tracing_subscriber::EnvFilter;
use zeck_core::{
    derive_accounts, estimate_birthday_from_date, validate_destination_address, RecoveryService,
    ScanConfig, ScanHandle, ScanPhase, SweepProposal, ZeckNetwork,
};

#[derive(Debug, Parser)]
#[command(name = "zeck", about = "Legacy ZecWallet Lite recovery preview tool")]
struct Cli {
    #[arg(long)]
    seed_file: Option<PathBuf>,

    #[arg(long, default_value = "https://mainnet.lightwalletd.com:9067")]
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

    let seed_phrase = load_seed_phrase(cli.seed_file)?;
    let account_count = cli.num_accounts.unwrap_or(cli.gap_limit.clamp(5, 20));

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
                        network,
                    },
                    seed_phrase,
                )
                .await?;

            let progress = wait_for_scan(&service, &handle).await?;
            print_scan_result(&progress);
        }
        Commands::Sweep {
            destination,
            confirm_sweep,
        } => {
            let address = validate_destination_address(&destination)?;
            println!(
                "Destination accepted: orchard={}, sapling={}, transparent={}",
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
                        network,
                    },
                    seed_phrase,
                )
                .await?;

            let progress = wait_for_scan(&service, &handle).await?;
            print_scan_result(&progress);

            let proposal = service.propose_sweep(&handle, &destination).await?;
            print_sweep_preview(&proposal);

            if confirm_sweep {
                let execution = service.execute_sweep(&handle, &destination).await;
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

fn load_seed_phrase(seed_file: Option<PathBuf>) -> Result<SecretString> {
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
        let message = progress
            .message
            .clone()
            .unwrap_or_else(|| format!("{:?}", progress.phase));
        spinner.set_message(message);

        match progress.phase {
            ScanPhase::Complete | ScanPhase::Cancelled | ScanPhase::Error => {
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
        println!("{}", summary.note);
    }

    if progress.accounts.is_empty() {
        println!("No accounts derived.");
        return;
    }

    println!();
    for account in &progress.accounts {
        println!(
            "Account {}  total={} zats  status={}",
            account.account_index, account.total_zatoshis, account.status
        );
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
    if let Some(warning) = &proposal.warning {
        println!("  warning: {warning}");
    }
}
