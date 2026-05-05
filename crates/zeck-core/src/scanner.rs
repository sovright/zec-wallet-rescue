//! Per-seed scanner actor for the multi-seed scan orchestrator.
//!
//! Each scanner owns one per-seed [`WalletDb`] and consumes compact blocks from
//! a shared block cache that is filled by a single [`crate::fetcher`] actor.
//! Scanners subscribe to the fetcher's `available_height` watch channel and
//! advance their wallet whenever new blocks become durable in the cache.
//!
//! ## Why not `zcash_client_backend::sync::run`?
//!
//! `sync::run` interleaves `download → scan → BlockCache::delete` per scan
//! range (see `zcash_client_backend-0.21.2/src/sync.rs` lines ~152, 203).
//! Running it once per seed against the *shared* cache would cause the first
//! seed's `delete` calls to evict blocks the second seed still needs. So this
//! actor replicates only the **scan** half of `sync::run` — calling
//! `update_chain_tip` and `scan_cached_blocks` against the per-seed wallet —
//! and never invokes `BlockCache::delete`. The shared cache outlives the run
//! and is managed elsewhere.
//!
//! `scan_cached_blocks` itself only uses the `BlockSource::with_blocks`
//! read-only trait method (verified in
//! `zcash_client_backend-0.21.2/src/data_api/chain.rs:584-700`), so passing
//! the shared cache directly is safe.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use rand_core::OsRng;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zcash_client_backend::data_api::{
    chain::{scan_cached_blocks, ChainState},
    WalletRead, WalletWrite,
};
use zcash_client_sqlite::{util::SystemClock, WalletDb};
use zcash_protocol::consensus::{BlockHeight, Network};

use crate::cache::SharedCacheReader;
use crate::models::DerivedAccount;
use crate::fetcher::CancellationToken;
use crate::workspace::RecoveryWorkspace;

/// Provider for [`ChainState`] at an arbitrary height.
///
/// `scan_cached_blocks` requires the final commitment-tree state of the block
/// immediately preceding the range being scanned, which in production comes
/// from lightwalletd's `get_tree_state` RPC. Threading the chain-state lookup
/// through this provider keeps the scanner agnostic to the transport: tests
/// can stub it with [`empty_chain_state_provider`] while the real orchestrator
/// will wrap a shared lightwalletd client.
pub type ChainStateFuture =
    Pin<Box<dyn std::future::Future<Output = Result<ChainState, String>> + Send>>;
pub type ChainStateProvider = Arc<dyn Fn(BlockHeight) -> ChainStateFuture + Send + Sync>;

/// Returns a [`ChainStateProvider`] that yields [`ChainState::empty`] at every
/// height. Useful for unit tests that don't actually drive `scan_cached_blocks`
/// (e.g. tests that gate on cancellation or birthday before any scan happens).
pub fn empty_chain_state_provider() -> ChainStateProvider {
    use zcash_primitives::block::BlockHash;
    Arc::new(|height: BlockHeight| {
        Box::pin(async move {
            Ok::<ChainState, String>(ChainState::empty(height, BlockHash([0u8; 32])))
        })
    })
}

/// Static configuration for a single scanner actor.
///
/// `seed_bytes` and `accounts` are intentionally part of the spec rather than
/// derived inside the scanner, because the orchestrator runs derivation and
/// account import once during the resolve phase and only hands the scanner a
/// pre-initialized workspace.
pub struct ScannerSpec {
    pub seed_index: usize,
    pub seed_fingerprint: String,
    pub seed_label: Option<String>,
    pub birthday: BlockHeight,
    pub workspace: RecoveryWorkspace,
    pub seed_bytes: secrecy::SecretVec<u8>,
    pub accounts: Vec<DerivedAccount>,
    pub gap_limit: u32,
    pub network: Network,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum SeedStatus {
    Pending,
    Scanning,
    Done,
    Cancelled,
    Failed(String),
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SeedProgress {
    pub seed_index: usize,
    pub seed_fingerprint: String,
    pub label: Option<String>,
    #[serde(with = "crate::models::serde_block_height")]
    pub birthday: BlockHeight,
    #[serde(with = "crate::models::serde_block_height::option")]
    pub fully_scanned_height: Option<BlockHeight>,
    pub status: SeedStatus,
    /// Total wallet balance (sapling + orchard + transparent) in zatoshis,
    /// populated by the multi-seed orchestrator's driver loop. `None` until
    /// the first driver tick after scanning starts; remains the most recent
    /// observed value once set.
    #[serde(default)]
    pub balance_zatoshis: Option<u64>,
}

pub struct ScannerHandle {
    pub progress: Arc<Mutex<SeedProgress>>,
    pub task: JoinHandle<Result<(), ScannerError>>,
    pub cancel: CancellationToken,
}

#[derive(Debug)]
pub enum ScannerError {
    Wallet(String),
    CacheRead(crate::cache::CacheError),
    ChainState(String),
    Cancelled,
}

impl std::fmt::Display for ScannerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wallet(msg) => write!(f, "scanner wallet error: {msg}"),
            Self::CacheRead(err) => write!(f, "scanner cache read error: {err}"),
            Self::ChainState(msg) => write!(f, "scanner chain state error: {msg}"),
            Self::Cancelled => write!(f, "scanner cancelled"),
        }
    }
}

impl std::error::Error for ScannerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CacheRead(err) => Some(err),
            _ => None,
        }
    }
}

/// Spawn a scanner actor.
///
/// The scanner runs until one of:
/// * the cancel token is flipped (returns [`ScannerError::Cancelled`]),
/// * the watch channel is closed AND the wallet's `fully_scanned_height`
///   matches the last broadcast height (returns `Ok`),
/// * a wallet, cache, or chain-state lookup error occurs.
pub fn spawn_scanner(
    spec: ScannerSpec,
    available_height: watch::Receiver<Option<BlockHeight>>,
    cache_reader: SharedCacheReader,
    chain_state_provider: ChainStateProvider,
    cancel: CancellationToken,
) -> ScannerHandle {
    let progress = Arc::new(Mutex::new(SeedProgress {
        seed_index: spec.seed_index,
        seed_fingerprint: spec.seed_fingerprint.clone(),
        label: spec.seed_label.clone(),
        birthday: spec.birthday,
        fully_scanned_height: None,
        status: SeedStatus::Pending,
        balance_zatoshis: None,
    }));
    let progress_for_task = progress.clone();
    let cancel_for_task = cancel.clone();

    let task = tokio::spawn(async move {
        run_scanner(
            spec,
            available_height,
            cache_reader,
            chain_state_provider,
            cancel_for_task,
            progress_for_task,
        )
        .await
    });

    ScannerHandle {
        progress,
        task,
        cancel,
    }
}

async fn run_scanner(
    spec: ScannerSpec,
    mut available_height: watch::Receiver<Option<BlockHeight>>,
    cache_reader: SharedCacheReader,
    chain_state_provider: ChainStateProvider,
    cancel: CancellationToken,
    progress: Arc<Mutex<SeedProgress>>,
) -> Result<(), ScannerError> {
    // Open the per-seed wallet DB. Mirrors the `WalletDb::for_path` invocation
    // in `scan.rs` (`run_wallet_sync` and `import_accounts`). The wallet is
    // assumed already initialized (accounts imported, schema migrated).
    let wallet_db_path = spec.workspace.wallet_db_path().to_path_buf();
    let mut wallet_db =
        WalletDb::for_path(&wallet_db_path, spec.network, SystemClock, OsRng).map_err(|err| {
            ScannerError::Wallet(format!(
                "opening wallet database {}: {err}",
                wallet_db_path.display()
            ))
        })?;

    set_status(&progress, SeedStatus::Scanning);
    update_fully_scanned_from_db(&progress, &wallet_db);

    loop {
        if cancel.is_cancelled() {
            set_status(&progress, SeedStatus::Cancelled);
            return Err(ScannerError::Cancelled);
        }

        // Snapshot the current available height (the watch channel always
        // exposes its latest value via `borrow()`).
        let available = *available_height.borrow();

        // If the fetcher hasn't broadcast a height yet, or the broadcast height
        // hasn't crossed our birthday, wait for the next change.
        let advance_target = match available {
            Some(h) if h >= spec.birthday => Some(h),
            _ => None,
        };

        if let Some(target) = advance_target {
            // Inform the wallet of the latest tip the cache has reached. This
            // lets `suggest_scan_ranges` propose ranges up to that tip.
            wallet_db.update_chain_tip(target).map_err(|err| {
                ScannerError::Wallet(format!("update_chain_tip({target}): {err}"))
            })?;

            let scan_ranges = wallet_db
                .suggest_scan_ranges()
                .map_err(|err| ScannerError::Wallet(format!("suggest_scan_ranges: {err}")))?;

            for range in scan_ranges {
                if cancel.is_cancelled() {
                    set_status(&progress, SeedStatus::Cancelled);
                    return Err(ScannerError::Cancelled);
                }

                // Clip the suggested range to `[max(birthday, ..) .. available]`.
                let raw_start = u32::from(range.block_range().start);
                let raw_end = u32::from(range.block_range().end);
                let start = raw_start.max(u32::from(spec.birthday));
                let end = raw_end.min(u32::from(target).saturating_add(1));
                if start >= end {
                    continue;
                }
                let from_height = BlockHeight::from_u32(start);
                let limit = (end - start) as usize;

                // `scan_cached_blocks` requires the chain state of the block
                // immediately preceding `from_height`. The provider is the
                // injection point for lightwalletd in production; tests stub
                // with `empty_chain_state_provider`.
                let prior_height = if start == 0 {
                    BlockHeight::from_u32(0)
                } else {
                    BlockHeight::from_u32(start - 1)
                };
                let from_state = (chain_state_provider)(prior_height)
                    .await
                    .map_err(ScannerError::ChainState)?;

                scan_cached_blocks(
                    &spec.network,
                    cache_reader.cache(),
                    &mut wallet_db,
                    from_height,
                    &from_state,
                    limit,
                )
                .map_err(|err| ScannerError::Wallet(format!("scan_cached_blocks: {err}")))?;

                update_fully_scanned_from_db(&progress, &wallet_db);
            }
        }

        // Wait for the next available_height update OR cancellation. We use
        // `tokio::select!` so a `cancel.cancel()` from another task is observed
        // promptly (within ~100ms via the polling fallback below) without
        // requiring the watch channel to fire.
        let outcome: Result<(), ()> = {
            let recv = available_height.changed();
            tokio::pin!(recv);
            tokio::select! {
                biased;
                res = &mut recv => match res {
                    Ok(()) => Ok(()),
                    Err(_) => Err(()),
                },
                _ = poll_cancel(&cancel) => {
                    set_status(&progress, SeedStatus::Cancelled);
                    return Err(ScannerError::Cancelled);
                }
            }
        };

        match outcome {
            Ok(()) => {
                // Loop and reprocess.
            }
            Err(()) => {
                // Sender dropped: the fetcher exited. If the wallet is
                // caught up to the last broadcast height, mark Done.
                let current = *available_height.borrow();
                let synced = read_fully_scanned(&wallet_db);
                let caught_up = match (current, synced) {
                    (Some(tip), Some(sh)) => sh >= tip,
                    (None, _) => true,
                    _ => false,
                };
                if caught_up {
                    set_status(&progress, SeedStatus::Done);
                    return Ok(());
                }
                set_status(
                    &progress,
                    SeedStatus::Failed("fetcher exited before scanner caught up".to_owned()),
                );
                return Err(ScannerError::Wallet(
                    "fetcher exited before scanner caught up".to_owned(),
                ));
            }
        }
    }
}

/// Coarse-grained cancellation poller. Mirrors `fetcher::sleep_or_cancel`'s
/// 100ms tick: that's plenty of resolution since cancellation is a
/// user-initiated event and the cost of a single extra batch is small.
async fn poll_cancel(cancel: &CancellationToken) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn set_status(progress: &Arc<Mutex<SeedProgress>>, status: SeedStatus) {
    if let Ok(mut guard) = progress.lock() {
        guard.status = status;
    }
}

fn update_fully_scanned_from_db<C, R>(
    progress: &Arc<Mutex<SeedProgress>>,
    wallet_db: &WalletDb<rusqlite::Connection, Network, C, R>,
) {
    if let Some(h) = read_fully_scanned(wallet_db) {
        if let Ok(mut guard) = progress.lock() {
            guard.fully_scanned_height = Some(h);
        }
    }
}

fn read_fully_scanned<C, R>(
    wallet_db: &WalletDb<rusqlite::Connection, Network, C, R>,
) -> Option<BlockHeight> {
    use zcash_client_backend::data_api::wallet::ConfirmationsPolicy;
    wallet_db
        .get_wallet_summary(ConfirmationsPolicy::MIN)
        .ok()
        .flatten()
        .map(|summary| summary.fully_scanned_height())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use secrecy::{SecretString, SecretVec};
    use tempfile::tempdir;
    use tokio::sync::watch;

    use super::*;
    use crate::cache::{SharedCacheReader, SharedCacheWriter};
    use crate::models::{RuntimeScanConfig, ZeckNetwork};
    use crate::workspace::{network_cache_db_path, network_cache_lock_path, RecoveryWorkspace};
    use zcash_client_sqlite::wallet::init::init_wallet_db;
    use zcash_primitives::block::BlockHash;
    use zcash_protocol::consensus::{NetworkUpgrade, Parameters};

    const SEED: &str = "abandon abandon abandon abandon abandon abandon \
                        abandon abandon abandon abandon abandon abandon \
                        abandon abandon abandon abandon abandon abandon \
                        abandon abandon abandon abandon abandon art";

    /// Build a minimal initialized workspace + cache. Does NOT import accounts:
    /// the tests in this module exercise the scanner's loop control flow
    /// (cancellation, birthday gating) which never actually drives
    /// `scan_cached_blocks` to completion. Driving real shielded scans against
    /// synthetic compact blocks would require a fully-formed sapling commitment
    /// tree and chain state, well beyond the surface this task introduces.
    fn build_workspace_and_cache(
        data_dir: PathBuf,
        birthday: u32,
        network: ZeckNetwork,
    ) -> (RecoveryWorkspace, SharedCacheReader, SharedCacheWriter) {
        let cfg = RuntimeScanConfig {
            seed_phrase: SecretString::new(SEED.to_owned()),
            birthday,
            num_accounts: Some(1),
            gap_limit: 5,
            lightwalletd_url: "https://example.invalid:443".to_owned(),
            data_dir: data_dir.clone(),
            network,
        };
        let workspace = RecoveryWorkspace::from_runtime(&cfg).expect("workspace");
        std::fs::create_dir_all(workspace.root()).unwrap();

        // Initialize the per-seed wallet DB schema (no accounts imported).
        let mut wallet_db = WalletDb::for_path(
            workspace.wallet_db_path(),
            crate::workspace::consensus_network(network),
            SystemClock,
            OsRng,
        )
        .expect("wallet open");
        init_wallet_db(&mut wallet_db, None).expect("init schema");
        drop(wallet_db);

        // Open the shared cache (writer reserves the lock; reader is
        // independent — same SQLite file via WAL).
        let cache_db = network_cache_db_path(workspace.data_dir(), network);
        let cache_lock = network_cache_lock_path(workspace.data_dir(), network);
        let writer = SharedCacheWriter::open(&cache_db, &cache_lock).expect("cache writer");
        let reader = SharedCacheReader::open(&cache_db).expect("cache reader");

        (workspace, reader, writer)
    }

    fn make_spec(workspace: RecoveryWorkspace, birthday: u32) -> ScannerSpec {
        ScannerSpec {
            seed_index: 0,
            seed_fingerprint: "test-fp".to_owned(),
            seed_label: Some("test".to_owned()),
            birthday: BlockHeight::from_u32(birthday),
            network: crate::workspace::consensus_network(workspace.network()),
            workspace,
            seed_bytes: SecretVec::new(vec![0u8; 64]),
            accounts: vec![],
            gap_limit: 5,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scanner_cancellation_breaks_loop() {
        let dir = tempdir().unwrap();
        let (workspace, reader, _writer) =
            build_workspace_and_cache(dir.path().to_path_buf(), 100, ZeckNetwork::Mainnet);
        let spec = make_spec(workspace, 100);

        let (_tx, rx) = watch::channel::<Option<BlockHeight>>(None);
        let cancel = CancellationToken::new();
        let handle = spawn_scanner(spec, rx, reader, empty_chain_state_provider(), cancel.clone());

        // Give the task a moment to enter its wait state.
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_millis(500), handle.task)
            .await
            .expect("task should exit promptly after cancel")
            .expect("join");
        assert!(matches!(result, Err(ScannerError::Cancelled)));
        assert_eq!(
            handle.progress.lock().unwrap().status,
            SeedStatus::Cancelled
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scanner_waits_until_birthday() {
        // Birthday is well above Sapling activation (so `update_chain_tip` does
        // not require any prior block metadata) but the available_height stays
        // below it for the first phase of the test.
        let activation = u32::from(
            zcash_protocol::consensus::Network::MainNetwork
                .activation_height(NetworkUpgrade::Sapling)
                .expect("sapling activation"),
        );
        let birthday = activation + 1_000;
        let dir = tempdir().unwrap();
        let (workspace, reader, _writer) = build_workspace_and_cache(
            dir.path().to_path_buf(),
            birthday,
            ZeckNetwork::Mainnet,
        );
        let spec = make_spec(workspace, birthday);

        let (tx, rx) = watch::channel::<Option<BlockHeight>>(None);
        let cancel = CancellationToken::new();
        let handle =
            spawn_scanner(spec, rx, reader, empty_chain_state_provider(), cancel.clone());

        // Push a height *below* birthday: scanner must NOT advance fully_scanned.
        tx.send(Some(BlockHeight::from_u32(birthday - 100))).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        {
            let progress = handle.progress.lock().unwrap();
            assert_eq!(progress.status, SeedStatus::Scanning);
            // No scan happened, so fully_scanned should be unchanged from
            // whatever the freshly-initialized wallet reports.
            // We just assert the scanner stayed alive without advancing past
            // birthday-related work.
            assert!(
                progress.fully_scanned_height.is_none()
                    || progress
                        .fully_scanned_height
                        .map(|h| u32::from(h) < birthday)
                        .unwrap_or(true),
                "fully_scanned_height should not jump past birthday before any scan",
            );
        }

        // Cancel cleanly; the point of this test is to prove the scanner did
        // not error out while waiting (gating works) — actually crossing the
        // birthday and calling `scan_cached_blocks` against synthetic blocks
        // would require a fully-formed sapling commitment frontier, which is
        // out of scope here.
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_millis(500), handle.task).await;
    }

    #[test]
    fn scanner_error_display_includes_kind() {
        let err = ScannerError::Cancelled;
        assert_eq!(err.to_string(), "scanner cancelled");
        let err = ScannerError::Wallet("boom".to_owned());
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn empty_chain_state_provider_returns_provided_height() {
        let provider = empty_chain_state_provider();
        let state = provider(BlockHeight::from_u32(123)).await.unwrap();
        assert_eq!(state.block_height(), BlockHeight::from_u32(123));
        assert_eq!(state.block_hash(), BlockHash([0u8; 32]));
    }
}
