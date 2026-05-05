use std::{
    collections::VecDeque,
    fs,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use dialoguer::Password;
use secrecy::SecretString;
use tracing_subscriber::EnvFilter;
use zeck_core::{
    derive_accounts, estimate_birthday_from_date, scanner::SeedStatus,
    validate_destination_address, MultiScanHandle, MultiSeedConfig, MultiSeedPhase,
    MultiSeedProgress, RecoveryService, ScanDiscovery, SeedEntry, SweepProposal, SweepRequest,
    ZeckNetwork,
};

#[derive(Debug, Parser)]
#[command(
    name = "zeck",
    about = "Legacy ZecWallet Lite recovery tool",
    long_about = "ZECK recovers funds from ZecWallet Lite wallets using a BIP-39 seed phrase.\n\
                  It derives keys, scans the Zcash blockchain via lightwalletd, and can sweep\n\
                  recovered funds to a new Unified Address.\n\n\
                  Multiple seeds can be scanned in one run by repeating --seed/--birthday or by \
                  passing --seeds-file with one entry per line.",
    version
)]
struct Cli {
    /// BIP-39 seed phrase (24 words). Repeat for multi-seed runs. Omit (and
    /// omit --seeds-file/--seed-file) to be prompted securely for one phrase.
    #[arg(long, action = ArgAction::Append, conflicts_with_all = ["seed_file", "seeds_file"])]
    seed: Vec<String>,

    /// Path to a plain-text file containing a single 24-word seed phrase.
    /// For multi-seed runs use --seeds-file instead.
    #[arg(long, conflicts_with_all = ["seed", "seeds_file", "birthday"])]
    seed_file: Option<PathBuf>,

    /// Path to a file with one seed entry per line. Each line is either
    /// `phrase` (auto-detect birthday) or `phrase | birthday` where birthday
    /// is `auto` or a u32 block height. Lines starting with `#` and blank
    /// lines are skipped.
    #[arg(long, conflicts_with_all = ["seed", "seed_file", "birthday", "birthday_date", "birthday_auto_detect"])]
    seeds_file: Option<PathBuf>,

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

    /// Wallet birthday as a block height, or `auto` to detect on-chain.
    /// Repeat once per --seed for multi-seed runs (or omit so all seeds
    /// auto-detect).
    #[arg(long, action = ArgAction::Append)]
    birthday: Vec<String>,

    /// Wallet creation date (YYYY-MM-DD). Single-seed only — used as the
    /// birthday when one --seed is supplied without --birthday.
    #[arg(long, conflicts_with_all = ["birthday_auto_detect", "seeds_file"])]
    birthday_date: Option<String>,

    /// Probe lightwalletd to auto-detect the wallet birthday from on-chain
    /// history. Single-seed convenience flag — equivalent to `--birthday auto`.
    #[arg(long, conflicts_with_all = ["birthday_date", "seeds_file"])]
    birthday_auto_detect: bool,

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
    /// Single-seed only — uses the first --seed when multiple are passed.
    ShowKeys,

    /// Scan the blockchain and report balances for all derived accounts.
    Scan,

    /// Re-run the scan and then sweep recovered funds from every funded seed
    /// to a single Unified Address.
    Sweep {
        /// Destination Unified Address (must include Orchard or Sapling receiver).
        #[arg(long)]
        destination: String,

        /// Optional memo attached to shielded outputs (max 512 bytes).
        #[arg(long)]
        memo: Option<String>,

        /// Maximum fee in ZEC (e.g. 0.001). Per-seed sweep is skipped if its
        /// estimated fee exceeds this.
        #[arg(long, value_parser = parse_zec_to_zatoshis)]
        max_fee: Option<u64>,

        /// Preview sweep proposals for every funded seed without broadcasting.
        #[arg(long, conflicts_with = "confirm_sweep")]
        dry_run: bool,

        /// Confirm you understand this is irreversible and broadcast every
        /// per-seed sweep. Failures on one seed do not block the rest.
        #[arg(long)]
        confirm_sweep: bool,
    },
}

/// Parsed input for one seed in a multi-seed run.
#[derive(Debug, Clone)]
struct SeedInput {
    phrase: String,
    /// `None` → auto-detect.
    birthday: Option<u32>,
    label: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose)?;

    let network: ZeckNetwork = cli.network.into();

    // Build the SeedInput list from whichever flag combination the user passed.
    let seed_inputs = collect_seed_inputs(&cli)?;

    if seed_inputs.is_empty() {
        bail!("no seed phrase provided");
    }

    if matches!(cli.command, Commands::Scan | Commands::Sweep { .. }) {
        eprintln!(
            "Note: scans can take hours for old wallets. Progress is saved \
             under {data_dir} after each batch — interrupt with Ctrl-C any time \
             and re-run with the same --data-dir, --network, --birthday, and \
             account-scan mode (the same --gap-limit, or the same --num-accounts) \
             to resume from the last persisted block per seed.",
            data_dir = cli.data_dir.display(),
        );
    }

    match cli.command {
        Commands::ShowKeys => {
            let account_count = cli.num_accounts.unwrap_or(20);
            // ShowKeys is single-seed: use the first entry. Warn if multiple.
            if seed_inputs.len() > 1 {
                eprintln!(
                    "Note: show-keys uses only the first --seed; the remaining {} were ignored.",
                    seed_inputs.len() - 1
                );
            }
            let phrase = SecretString::new(seed_inputs[0].phrase.clone());
            let accounts = derive_accounts(&phrase, network, account_count)?;
            for account in accounts {
                println!("━━━ Account {} ━━━", account.index);
                println!("  Unified address     {}", account.unified_address);
                println!("  Orchard path        {}", account.orchard_path);
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
            let entries = build_seed_entries(&seed_inputs);
            let handle = service
                .start_multi_scan(
                    entries,
                    MultiSeedConfig {
                        network,
                        lightwalletd_url: cli.lightwalletd_url.clone(),
                        data_dir: cli.data_dir.clone(),
                        gap_limit: cli.gap_limit,
                        num_accounts: cli.num_accounts,
                    },
                )
                .await?;

            let progress = wait_for_multi_scan(&service, &handle).await?;
            print_multi_scan_result(&progress);
            notify_scan_complete(&progress);
            match progress.phase {
                MultiSeedPhase::Cancelled => std::process::exit(130),
                MultiSeedPhase::Failed(_) => bail!("recovery scan failed"),
                _ => {}
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
            let entries = build_seed_entries(&seed_inputs);
            let handle = service
                .start_multi_scan(
                    entries,
                    MultiSeedConfig {
                        network,
                        lightwalletd_url: cli.lightwalletd_url.clone(),
                        data_dir: cli.data_dir.clone(),
                        gap_limit: cli.gap_limit,
                        num_accounts: cli.num_accounts,
                    },
                )
                .await?;

            let progress = wait_for_multi_scan(&service, &handle).await?;
            print_multi_scan_result(&progress);
            notify_scan_complete(&progress);
            match progress.phase {
                MultiSeedPhase::Cancelled => std::process::exit(130),
                MultiSeedPhase::Failed(_) => bail!("recovery scan failed"),
                _ => {}
            }

            // Identify funded seeds. Per-seed balance comes from per_seed
            // (populated by the orchestrator) — non-None and > 0.
            let funded: Vec<&zeck_core::scanner::SeedProgress> = progress
                .per_seed
                .iter()
                .filter(|s| s.balance_zatoshis.unwrap_or(0) > 0)
                .collect();

            let total_zats: u64 = funded
                .iter()
                .map(|s| s.balance_zatoshis.unwrap_or(0))
                .sum();
            println!();
            println!(
                "{} of {} seeds funded — {}. Destination: {}",
                funded.len(),
                progress.per_seed.len(),
                format_zec(total_zats),
                destination,
            );

            if funded.is_empty() {
                println!("Nothing to sweep.");
                return Ok(());
            }

            let request = SweepRequest {
                destination: destination.clone(),
                memo: memo.clone(),
                max_fee_zatoshis: max_fee,
            };

            // Propose per-seed.
            for seed in &funded {
                println!();
                println!(
                    "── seed [{:>2}] {} (fp {}) ──",
                    seed.seed_index,
                    seed.label.as_deref().unwrap_or("(unlabelled)"),
                    short_fp(&seed.seed_fingerprint),
                );
                match service
                    .propose_sweep_for_seed(&handle, seed.seed_index, request.clone())
                    .await
                {
                    Ok(proposal) => print_sweep_preview(&proposal),
                    Err(err) => eprintln!("  proposal failed: {err}"),
                }
            }

            if dry_run {
                println!();
                println!("Dry run complete. Re-run with --confirm-sweep to broadcast.");
                return Ok(());
            }

            if !confirm_sweep {
                println!();
                println!(
                    "Re-run with --dry-run to preview, or --confirm-sweep to broadcast."
                );
                return Ok(());
            }

            // Execute per-seed. Failures on one seed don't block others.
            println!();
            println!("Broadcasting sweep transactions…");
            let mut any_failure = false;
            for seed in &funded {
                println!();
                println!(
                    "── executing sweep for seed [{:>2}] {} ──",
                    seed.seed_index,
                    seed.label.as_deref().unwrap_or("(unlabelled)"),
                );
                match service
                    .execute_sweep_for_seed(&handle, seed.seed_index, request.clone())
                    .await
                {
                    Ok(results) => {
                        for result in &results {
                            println!(
                                "  account {}  {}  {}",
                                result.source_account, result.status, result.detail
                            );
                        }
                    }
                    Err(err) => {
                        any_failure = true;
                        eprintln!("  sweep failed for seed {}: {}", seed.seed_index, err);
                    }
                }
            }

            println!();
            if any_failure {
                println!("Sweep complete with errors — see messages above.");
                std::process::exit(1);
            } else {
                println!("Sweep complete.");
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

/// Resolve all seed/birthday CLI flags into a normalized list of seed inputs.
/// Validates mutual exclusion and count matching. Prompts interactively only
/// when no seed source flag is given (legacy single-seed UX).
fn collect_seed_inputs(cli: &Cli) -> Result<Vec<SeedInput>> {
    if let Some(path) = &cli.seeds_file {
        let entries = parse_seeds_file(path)
            .with_context(|| format!("failed to parse seeds file {}", path.display()))?;
        if entries.is_empty() {
            bail!("seeds file {} contained no entries", path.display());
        }
        return Ok(entries);
    }

    if let Some(path) = &cli.seed_file {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read seed file {}", path.display()))?;
        let phrase = contents.trim().to_owned();
        if phrase.is_empty() {
            bail!("seed file {} was empty", path.display());
        }
        let birthday = resolve_single_birthday_flag(cli)?;
        return Ok(vec![SeedInput {
            phrase,
            birthday,
            label: None,
        }]);
    }

    if !cli.seed.is_empty() {
        // Multi-seed via repeated --seed. Validate --birthday count.
        let birthdays = if cli.birthday.is_empty() {
            vec![None; cli.seed.len()]
        } else if cli.birthday.len() == cli.seed.len() {
            cli.birthday
                .iter()
                .map(|s| parse_birthday_token(s))
                .collect::<Result<Vec<_>>>()?
        } else {
            bail!(
                "--birthday count ({}) must match --seed count ({}) (or omit --birthday so all seeds auto-detect)",
                cli.birthday.len(),
                cli.seed.len(),
            );
        };

        // --birthday-date / --birthday-auto-detect are single-seed conveniences.
        if cli.seed.len() == 1 && cli.birthday.is_empty() {
            // Apply legacy single-seed birthday flags.
            let birthday = resolve_single_birthday_flag(cli)?;
            return Ok(vec![SeedInput {
                phrase: cli.seed[0].trim().to_owned(),
                birthday,
                label: None,
            }]);
        }

        if cli.seed.len() > 1
            && (cli.birthday_date.is_some() || cli.birthday_auto_detect)
        {
            bail!(
                "--birthday-date and --birthday-auto-detect are single-seed only; \
                 use repeated --birthday auto / --birthday <height> for multi-seed runs"
            );
        }

        return Ok(cli
            .seed
            .iter()
            .zip(birthdays)
            .map(|(phrase, birthday)| SeedInput {
                phrase: phrase.trim().to_owned(),
                birthday,
                label: None,
            })
            .collect());
    }

    // Interactive prompt — single seed only.
    let phrase = Password::new()
        .with_prompt("Enter your 24-word seed phrase")
        .allow_empty_password(false)
        .interact()
        .context("failed to read seed phrase from terminal")?
        .trim()
        .to_owned();
    let birthday = resolve_single_birthday_flag(cli)?;
    Ok(vec![SeedInput {
        phrase,
        birthday,
        label: None,
    }])
}

/// For single-seed legacy paths: turn the `--birthday <height|auto>`,
/// `--birthday-date`, and `--birthday-auto-detect` flags into an
/// `Option<u32>` (None = auto-detect at scan time).
fn resolve_single_birthday_flag(cli: &Cli) -> Result<Option<u32>> {
    if cli.birthday_auto_detect {
        return Ok(None);
    }
    if let Some(date) = &cli.birthday_date {
        return Ok(Some(estimate_birthday_from_date(date)?));
    }
    if cli.birthday.is_empty() {
        // Legacy default: pre-Sapling-ish; treat as None so the resolver
        // falls back to Sapling activation rather than a hardcoded constant.
        // To preserve backward-compat with the old default of 419_200, we
        // keep that as the explicit value.
        return Ok(Some(419_200));
    }
    if cli.birthday.len() > 1 {
        bail!(
            "--birthday repeated {} times but only one --seed provided",
            cli.birthday.len()
        );
    }
    parse_birthday_token(&cli.birthday[0])
}

/// Parse a single `--birthday` value. `auto` → `None`; otherwise a u32 height.
fn parse_birthday_token(token: &str) -> Result<Option<u32>> {
    let trimmed = token.trim();
    if trimmed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let height: u32 = trimmed
        .parse()
        .with_context(|| format!("invalid --birthday value `{trimmed}` (expected u32 or `auto`)"))?;
    Ok(Some(height))
}

/// Parse a `--seeds-file` file: one entry per line. `phrase` or
/// `phrase | birthday` (auto / u32). `#` and blank lines skipped.
fn parse_seeds_file(path: &Path) -> Result<Vec<SeedInput>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    parse_seeds_file_contents(&contents)
}

fn parse_seeds_file_contents(contents: &str) -> Result<Vec<SeedInput>> {
    let mut out = Vec::new();
    for (lineno, raw) in contents.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('|').collect();
        match parts.as_slice() {
            [phrase] => {
                out.push(SeedInput {
                    phrase: phrase.trim().to_owned(),
                    birthday: None,
                    label: None,
                });
            }
            [phrase, birthday] => {
                let birthday = parse_birthday_token(birthday).with_context(|| {
                    format!("invalid birthday on line {}", lineno + 1)
                })?;
                out.push(SeedInput {
                    phrase: phrase.trim().to_owned(),
                    birthday,
                    label: None,
                });
            }
            _ => {
                bail!(
                    "line {} has too many `|` separators (expected `phrase` or `phrase | birthday`)",
                    lineno + 1
                );
            }
        }
    }
    Ok(out)
}

fn build_seed_entries(inputs: &[SeedInput]) -> Vec<SeedEntry> {
    inputs
        .iter()
        .map(|i| SeedEntry {
            phrase: SecretString::new(i.phrase.clone()),
            birthday: i.birthday,
            label: i.label.clone(),
        })
        .collect()
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

/// Render a multi-seed scan progress in place (TTY) or line-by-line
/// (piped/redirected). Polls `service.get_multi_scan_progress` every 250 ms.
async fn wait_for_multi_scan(
    service: &RecoveryService,
    handle: &MultiScanHandle,
) -> Result<MultiSeedProgress> {
    let tty = std::io::stderr().is_terminal();
    let mut last_lines_drawn: usize = 0;
    let mut eta = EtaTracker::new();
    let started_at = Instant::now();
    let mut discoveries_seen: usize = 0;

    loop {
        let progress = service.get_multi_scan_progress(handle).await?;

        // Self-heal cursor (append-only contract — clamp defensively).
        if discoveries_seen > progress.discoveries.len() {
            discoveries_seen = progress.discoveries.len();
        }
        if progress.discoveries.len() > discoveries_seen {
            // Print discoveries above the table. In TTY mode we first
            // clear the previous table, emit the discoveries, then redraw.
            if tty && last_lines_drawn > 0 {
                clear_lines(last_lines_drawn);
                last_lines_drawn = 0;
            }
            for d in &progress.discoveries[discoveries_seen..] {
                eprintln!("{}", format_discovery(d));
            }
            discoveries_seen = progress.discoveries.len();
        }

        eta.observe(progress.blocks_scanned, total_blocks_target(&progress));

        let eta_label = match eta.estimate(started_at.elapsed()) {
            EtaEstimate::Warmup => "estimating…".to_string(),
            EtaEstimate::Range(text) => text,
            EtaEstimate::Done => String::new(),
        };
        let era_label = progress
            .synced_to_height
            .map(u32::from)
            .and_then(|h| era_hint(h as u64))
            .unwrap_or_default();

        let lines = render_multi_seed_lines(&progress, &eta_label, &era_label);

        if tty {
            if last_lines_drawn > 0 {
                clear_lines(last_lines_drawn);
            }
            for line in &lines {
                eprintln!("{line}");
            }
            last_lines_drawn = lines.len();
        } else {
            // Piped/redirected: append a compact one-liner per tick.
            eprintln!(
                "[{:?}] {}/{} seeds done · scanned {} blocks · {}",
                progress.phase,
                progress
                    .per_seed
                    .iter()
                    .filter(|s| matches!(s.status, SeedStatus::Done))
                    .count(),
                progress.per_seed.len(),
                progress.blocks_scanned,
                eta_label,
            );
        }

        if matches!(
            progress.phase,
            MultiSeedPhase::Completed | MultiSeedPhase::Cancelled | MultiSeedPhase::Failed(_)
        ) {
            return Ok(progress);
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Estimate the run's overall block-scan target by summing per-seed deltas
/// (target_tip - birthday) where known. Falls back to the largest single-seed
/// estimate if `target_tip` is unset.
fn total_blocks_target(progress: &MultiSeedProgress) -> u64 {
    let Some(tip) = progress.fetcher.target_tip.map(u32::from) else {
        return 0;
    };
    progress
        .per_seed
        .iter()
        .map(|s| {
            let bday = u32::from(s.birthday);
            tip.saturating_sub(bday) as u64
        })
        .sum()
}

/// Build the lines for the per-seed table (header + one row per seed +
/// fetcher footer). Returned as plain strings so the renderer can emit them
/// in either TTY or piped mode.
fn render_multi_seed_lines(
    progress: &MultiSeedProgress,
    eta_label: &str,
    era_label: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!(
        "{:<6}{:<14}{:>12}{:>14}{:>10}  status",
        "seed", "label", "birthday", "scanned", "balance"
    ));
    for seed in &progress.per_seed {
        let label = seed
            .label
            .clone()
            .unwrap_or_else(|| format!("(seed-{})", seed.seed_index));
        let label = truncate(&label, 14);
        let birthday = format_thousands(u32::from(seed.birthday) as u64);
        let scanned = match seed.fully_scanned_height {
            Some(h) => format_thousands(u32::from(h) as u64),
            None => "—".to_string(),
        };
        let balance = match seed.balance_zatoshis {
            Some(b) if b > 0 => format_zec(b),
            Some(_) => "0".to_string(),
            None => "—".to_string(),
        };
        let status = match &seed.status {
            SeedStatus::Pending => "pending".to_string(),
            SeedStatus::Scanning => "scanning".to_string(),
            SeedStatus::Done => "done".to_string(),
            SeedStatus::Cancelled => "cancelled".to_string(),
            SeedStatus::Failed(msg) => format!("failed: {}", truncate(msg, 40)),
        };
        out.push(format!(
            "[{:>2}]  {:<14}{:>12}{:>14}{:>10}  {}",
            seed.seed_index, label, birthday, scanned, balance, status
        ));
    }

    let fetcher = &progress.fetcher;
    let dl = fetcher
        .downloaded_to_height
        .map(|h| format_thousands(u32::from(h) as u64))
        .unwrap_or_else(|| "—".to_string());
    let tip = fetcher
        .target_tip
        .map(|h| format_thousands(u32::from(h) as u64))
        .unwrap_or_else(|| "—".to_string());
    out.push(format!(
        "fetcher  downloaded {} / target {}  retries {}",
        dl, tip, fetcher.retry_count
    ));

    let phase = match &progress.phase {
        MultiSeedPhase::Resolving => "resolving".to_string(),
        MultiSeedPhase::Scanning => "scanning".to_string(),
        MultiSeedPhase::Completed => "completed".to_string(),
        MultiSeedPhase::Cancelled => "cancelled".to_string(),
        MultiSeedPhase::Failed(msg) => format!("failed: {}", truncate(msg, 60)),
    };
    let mut footer = format!("phase {phase}");
    if !era_label.is_empty() {
        footer.push_str(&format!(" · scanning ~{era_label}"));
    }
    if !eta_label.is_empty() {
        footer.push_str(&format!(" · {eta_label}"));
    }
    out.push(footer);

    out
}

/// Move cursor up `n` lines and clear each. Uses raw ANSI so we don't need
/// crossterm. No-op when `n == 0`.
fn clear_lines(n: usize) {
    if n == 0 {
        return;
    }
    let mut stderr = std::io::stderr();
    // Move up N lines, then for each line: clear the line + move down.
    // Final positioning leaves the cursor at the top of the previous block.
    let _ = write!(stderr, "\x1b[{n}A");
    for _ in 0..n {
        let _ = write!(stderr, "\x1b[2K\x1b[1B");
    }
    // Move back up to the top of the cleared block so the caller's
    // subsequent prints overwrite.
    let _ = write!(stderr, "\x1b[{n}A");
    let _ = stderr.flush();
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn format_thousands(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*c as char);
    }
    out
}

fn short_fp(fp: &str) -> String {
    if fp.len() <= 12 {
        fp.to_string()
    } else {
        format!("{}…{}", &fp[..6], &fp[fp.len() - 4..])
    }
}

/// Sliding-window ETA tracker that ignores the noisy first few seconds and
/// returns a rounded range rather than a false-precision point estimate.
struct EtaTracker {
    samples: VecDeque<(Instant, u64)>,
    last_total: u64,
}

enum EtaEstimate {
    Warmup,
    Range(String),
    Done,
}

impl EtaTracker {
    const WARMUP: Duration = Duration::from_secs(15);
    const WINDOW: Duration = Duration::from_secs(45);

    fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            last_total: 0,
        }
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
    let year = SAPLING_YEAR + (elapsed_years + 0.18) as i32;
    Some(year.to_string())
}

fn format_discovery(discovery: &ScanDiscovery) -> String {
    format!(
        "[scanned through block {}] account {}  +{} {}",
        discovery.at_block_height,
        discovery.account_index,
        format_zec(discovery.zatoshis),
        discovery.pool.label(),
    )
}

fn print_multi_scan_result(progress: &MultiSeedProgress) {
    println!();
    println!("Phase: {:?}", progress.phase);

    if !progress.warnings.is_empty() {
        for w in &progress.warnings {
            println!("Warning: {w:?}");
        }
    }

    if progress.per_seed.is_empty() {
        println!("No seeds scanned.");
        return;
    }

    println!();
    println!(
        "{:<6}{:<14}{:>12}{:>14}{:>16}  status",
        "seed", "label", "birthday", "scanned", "balance"
    );
    println!("{}", "─".repeat(80));
    for seed in &progress.per_seed {
        let label = seed
            .label
            .clone()
            .unwrap_or_else(|| format!("(seed-{})", seed.seed_index));
        let scanned = seed
            .fully_scanned_height
            .map(|h| format_thousands(u32::from(h) as u64))
            .unwrap_or_else(|| "—".to_string());
        let balance = match seed.balance_zatoshis {
            Some(b) => format_zec(b),
            None => "—".to_string(),
        };
        let status = match &seed.status {
            SeedStatus::Pending => "pending".to_string(),
            SeedStatus::Scanning => "scanning".to_string(),
            SeedStatus::Done => "done".to_string(),
            SeedStatus::Cancelled => "cancelled".to_string(),
            SeedStatus::Failed(msg) => format!("failed: {msg}"),
        };
        println!(
            "[{:>2}]  {:<14}{:>12}{:>14}{:>16}  {}",
            seed.seed_index,
            truncate(&label, 14),
            format_thousands(u32::from(seed.birthday) as u64),
            scanned,
            balance,
            status,
        );
    }
    println!("{}", "─".repeat(80));
    println!();
    println!("{}", multi_scan_completion_summary(progress));
    println!();
}

fn print_sweep_preview(proposal: &SweepProposal) {
    println!("Sweep preview:");
    println!("  Send:        {}", format_zec(proposal.total_send_zatoshis));
    println!("  Fee:         {}", format_zec(proposal.total_fee_zatoshis));
    println!(
        "  Net receive: {}",
        format_zec(proposal.net_received_zatoshis)
    );

    if !proposal.transactions.is_empty() {
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
        println!("  Warning: {warning}");
    }
}

/// Try to grab the user's attention when a long-running scan finishes. Best
/// effort: terminal bell always; OS-level notification on macOS/Linux/Windows.
fn notify_scan_complete(progress: &MultiSeedProgress) {
    let title = match &progress.phase {
        MultiSeedPhase::Completed => "ZECK scan complete",
        MultiSeedPhase::Cancelled => "ZECK scan cancelled",
        MultiSeedPhase::Failed(_) => "ZECK scan failed",
        _ => return,
    };

    let body = multi_scan_completion_summary(progress);

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
            title = powershell_quote(title),
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
}

fn multi_scan_completion_summary(progress: &MultiSeedProgress) -> String {
    if let MultiSeedPhase::Failed(msg) = &progress.phase {
        return msg.clone();
    }
    if matches!(progress.phase, MultiSeedPhase::Cancelled) {
        return "Scan stopped before completion. Re-run with the same flags to resume."
            .to_string();
    }
    let funded: Vec<_> = progress
        .per_seed
        .iter()
        .filter(|s| s.balance_zatoshis.unwrap_or(0) > 0)
        .collect();
    let total: u64 = funded.iter().map(|s| s.balance_zatoshis.unwrap_or(0)).sum();
    let total_seeds = progress.per_seed.len();
    if funded.is_empty() {
        return format!("0 of {total_seeds} seeds funded — no funds were found.");
    }
    let zec = format_zec(total);
    format!("{} of {} seeds funded — {}.", funded.len(), total_seeds, zec)
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

#[cfg(target_os = "windows")]
fn powershell_quote(input: &str) -> String {
    let escaped: String = input
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c == '\'' { "''".to_string() } else { c.to_string() })
        .collect();
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

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
        assert_eq!(format_eta_range(7 * 60), "about 5 minutes remaining");
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
    fn format_zec_whole() {
        assert_eq!(format_zec(100_000_000), "1 ZEC");
    }

    #[test]
    fn format_zec_fractional() {
        assert_eq!(format_zec(50_000_000), "0.50000000 ZEC");
    }

    #[test]
    fn format_zec_one_zatoshi() {
        assert_eq!(format_zec(1), "0.00000001 ZEC");
    }

    #[test]
    fn format_zec_zero() {
        assert_eq!(format_zec(0), "0 ZEC");
    }

    #[test]
    fn format_zec_large() {
        assert_eq!(format_zec(2_100_000_000_000_000), "21000000 ZEC");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn powershell_quote_simple() {
        assert_eq!(powershell_quote("hello"), "'hello'");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn powershell_quote_single_quote_escaped() {
        assert_eq!(powershell_quote("it's done"), "'it''s done'");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn powershell_quote_strips_control_chars() {
        assert_eq!(powershell_quote("abc\x00def"), "'abcdef'");
    }

    // ── seeds-file parsing ──────────────────────────────────────────────────

    #[test]
    fn parse_seeds_file_phrase_only() {
        let input = "abandon abandon abandon\nzoo zoo zoo zoo\n";
        let entries = parse_seeds_file_contents(input).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].phrase, "abandon abandon abandon");
        assert_eq!(entries[0].birthday, None);
        assert_eq!(entries[1].phrase, "zoo zoo zoo zoo");
        assert_eq!(entries[1].birthday, None);
    }

    #[test]
    fn parse_seeds_file_phrase_and_birthday() {
        let input = "abandon abandon | 2400000\nzoo zoo zoo | auto\n";
        let entries = parse_seeds_file_contents(input).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].birthday, Some(2_400_000));
        assert_eq!(entries[1].birthday, None);
    }

    #[test]
    fn parse_seeds_file_skips_comments_and_blanks() {
        let input = "# header comment\n\nabandon abandon\n   \n# trailing\nzoo zoo | 100\n";
        let entries = parse_seeds_file_contents(input).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].phrase, "abandon abandon");
        assert_eq!(entries[1].birthday, Some(100));
    }

    #[test]
    fn parse_seeds_file_rejects_too_many_pipes() {
        let input = "abandon | 100 | extra\n";
        let err = parse_seeds_file_contents(input).unwrap_err();
        assert!(format!("{err}").contains("too many"), "got: {err}");
    }

    #[test]
    fn parse_seeds_file_rejects_invalid_birthday() {
        let input = "abandon abandon | not-a-number\n";
        let err = parse_seeds_file_contents(input).unwrap_err();
        assert!(format!("{err:#}").contains("invalid"), "got: {err:#}");
    }

    // ── clap validation ─────────────────────────────────────────────────────

    #[test]
    fn cli_rejects_seeds_file_combined_with_seed_args() {
        let cmd = Cli::command();
        let res = cmd.clone().try_get_matches_from([
            "zeck",
            "--seeds-file",
            "x.txt",
            "--seed",
            "abandon",
            "scan",
        ]);
        assert!(res.is_err());

        let res = cmd.clone().try_get_matches_from([
            "zeck",
            "--seeds-file",
            "x.txt",
            "--seed-file",
            "x.txt",
            "scan",
        ]);
        assert!(res.is_err());

        let res = cmd.clone().try_get_matches_from([
            "zeck",
            "--seeds-file",
            "x.txt",
            "--birthday",
            "100",
            "scan",
        ]);
        assert!(res.is_err());
    }

    #[test]
    fn cli_validates_birthday_count_matches_seed_count() {
        // Two seeds + one birthday → collect_seed_inputs rejects.
        let cli = Cli::try_parse_from([
            "zeck",
            "--seed",
            "abandon abandon",
            "--seed",
            "zoo zoo",
            "--birthday",
            "100",
            "scan",
        ])
        .unwrap();
        let err = collect_seed_inputs(&cli).unwrap_err();
        assert!(format!("{err:#}").contains("must match"), "got: {err:#}");

        // Two seeds + two birthdays → ok.
        let cli = Cli::try_parse_from([
            "zeck",
            "--seed",
            "abandon abandon",
            "--seed",
            "zoo zoo",
            "--birthday",
            "100",
            "--birthday",
            "auto",
            "scan",
        ])
        .unwrap();
        let inputs = collect_seed_inputs(&cli).unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].birthday, Some(100));
        assert_eq!(inputs[1].birthday, None);

        // Two seeds + zero birthdays → ok (all auto).
        let cli = Cli::try_parse_from([
            "zeck",
            "--seed",
            "abandon abandon",
            "--seed",
            "zoo zoo",
            "scan",
        ])
        .unwrap();
        let inputs = collect_seed_inputs(&cli).unwrap();
        assert_eq!(inputs.len(), 2);
        assert!(inputs.iter().all(|i| i.birthday.is_none()));
    }
}
