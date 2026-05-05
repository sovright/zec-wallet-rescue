//! Block-fetcher actor for the multi-seed scan orchestrator.
//!
//! The fetcher owns a lightwalletd client and a [`SharedCacheWriter`], and is
//! responsible for downloading compact blocks from a starting height up to the
//! current chain tip in batches of [`SYNC_BATCH_SIZE`] blocks. Each batch is
//! written to the shared block cache and the latest available height is
//! broadcast on a [`watch`] channel so per-seed scanners can advance as soon as
//! new blocks are durable.
//!
//! ## Why a separate fetcher?
//!
//! `zcash_client_backend::sync::run` interleaves download → scan → **delete**
//! per scan range. Running it once per seed against a shared cache would cause
//! the first seed's `BlockCache::delete` calls to evict blocks the second seed
//! still needs. By extracting just the download primitive into this actor,
//! the cache becomes append-only (relative to the fetcher) and seeds can scan
//! the same blocks concurrently or sequentially without re-downloading.
//!
//! Wiring of per-seed scanners against this fetcher (and suppression of
//! `BlockCache::delete` for the shared writer) lives in a later orchestrator
//! task; this module only defines the actor surface.
//!
//! ## Reconnect semantics
//!
//! Mirrors `run_wallet_sync_with_retry` in [`crate::scan`]: up to
//! [`MAX_FETCHER_RETRIES`] reconnects with [`FETCHER_RETRY_DELAY_SECS`] backoff
//! between attempts. The error-string heuristic (GoAway / TLS close_notify /
//! TimedOut / UnexpectedEof / h2 protocol error / generic transport error) is
//! kept in lockstep with that helper.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use tokio::sync::watch;
use tonic::transport::Channel;
use tracing::warn;
use zcash_client_backend::{
    data_api::chain::BlockCache,
    proto::service::{compact_tx_streamer_client::CompactTxStreamerClient, BlockId, BlockRange, ChainSpec},
};
use zcash_protocol::consensus::BlockHeight;

use crate::cache::{CacheError, SharedCacheWriter};
use crate::lightwalletd::probe_lightwalletd_endpoints;

/// Number of blocks downloaded per `get_block_range` request and per
/// `BlockCache::insert` flush. Matches `crate::scan::SYNC_BATCH_SIZE` so the
/// shared cache sees identically-sized batches whether they arrive from the
/// fetcher or from a one-shot legacy sync path.
pub const SYNC_BATCH_SIZE: u32 = 1_000;

/// Maximum reconnect attempts on transient transport errors. Mirrors
/// `crate::scan::MAX_SYNC_RETRIES`.
pub const MAX_FETCHER_RETRIES: u32 = 10;

/// Backoff between reconnect attempts. Mirrors `crate::scan::SYNC_RETRY_DELAY_SECS`.
pub const FETCHER_RETRY_DELAY_SECS: u64 = 5;

/// Errors that terminate the fetcher actor.
#[derive(Debug)]
pub enum FetcherError {
    /// Transport-level failure that survived [`MAX_FETCHER_RETRIES`] reconnects,
    /// or a non-transport gRPC failure (e.g. a misbehaving server).
    Transport(String),
    /// Failure writing a downloaded batch into the shared cache.
    CacheWrite(CacheError),
    /// The fetcher was cancelled via its [`CancellationToken`] before reaching
    /// the chain tip.
    Cancelled,
}

impl std::fmt::Display for FetcherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(msg) => write!(f, "fetcher transport error: {msg}"),
            Self::CacheWrite(err) => write!(f, "fetcher cache write error: {err}"),
            Self::Cancelled => write!(f, "fetcher cancelled"),
        }
    }
}

impl std::error::Error for FetcherError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CacheWrite(err) => Some(err),
            _ => None,
        }
    }
}

/// Lightweight cancellation token: the fetcher checks this at every batch
/// boundary. Implemented as `Arc<AtomicBool>` to match the existing
/// `ScanTaskState.cancelled` pattern in `scan.rs` and avoid adding a
/// `tokio-util` dependency.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Final outcome of a successful fetcher run.
#[derive(Debug, Clone)]
pub struct FetcherSummary {
    /// The chain tip the fetcher caught up to before exiting.
    pub final_tip: BlockHeight,
    /// Number of reconnects performed during the run.
    pub retry_count: u32,
}

/// Periodic progress payload (currently exposed only via `available_height`,
/// retained as a struct so future telemetry can hang off it without breaking
/// the actor surface).
#[derive(Debug, Clone)]
pub struct FetcherProgress {
    pub downloaded_to_height: Option<BlockHeight>,
    pub target_tip: Option<BlockHeight>,
    pub retry_count: u32,
}

/// Handle returned by [`spawn_fetcher`].
pub struct FetcherHandle {
    /// Highest block height present in the shared cache. Updated after every
    /// successful batch insert.
    pub available_height: watch::Receiver<Option<BlockHeight>>,
    /// The background fetcher task. Awaitable for the final summary.
    pub task: tokio::task::JoinHandle<Result<FetcherSummary, FetcherError>>,
    /// Shared cancellation token; clone and call `.cancel()` to request a
    /// clean shutdown at the next batch boundary.
    pub cancel: CancellationToken,
}

/// Configuration for [`spawn_fetcher`].
pub struct FetcherConfig {
    /// First block height to download (inclusive).
    pub start_height: BlockHeight,
    /// Comma/semicolon/whitespace-separated lightwalletd endpoints, used both
    /// for the initial connect and for reconnect probes after transport errors.
    pub lightwalletd_endpoints: String,
}

/// Spawn a fetcher actor.
///
/// Owns the supplied lightwalletd `client` and `cache_writer` for the lifetime
/// of the background task. The caller drives the fetcher via the returned
/// [`FetcherHandle`]:
///
/// * Read `available_height` to learn how far the cache has advanced.
/// * Call `cancel.cancel()` for an early shutdown.
/// * `await` `task` for the final [`FetcherSummary`] (or [`FetcherError`]).
pub fn spawn_fetcher(
    client: CompactTxStreamerClient<Channel>,
    cache_writer: SharedCacheWriter,
    config: FetcherConfig,
) -> FetcherHandle {
    let (tx, rx) = watch::channel::<Option<BlockHeight>>(None);
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();

    let task = tokio::spawn(async move {
        run_fetcher(client, cache_writer, config, tx, cancel_for_task).await
    });

    FetcherHandle {
        available_height: rx,
        task,
        cancel,
    }
}

async fn run_fetcher(
    mut client: CompactTxStreamerClient<Channel>,
    cache_writer: SharedCacheWriter,
    config: FetcherConfig,
    available_tx: watch::Sender<Option<BlockHeight>>,
    cancel: CancellationToken,
) -> Result<FetcherSummary, FetcherError> {
    let mut next_height = u32::from(config.start_height);
    let mut retry_count: u32 = 0;
    let endpoints = config.lightwalletd_endpoints;

    // Discover chain tip first so we have a target.
    let mut tip = fetch_tip_with_retry(&mut client, &endpoints, &cancel, &mut retry_count).await?;

    loop {
        if cancel.is_cancelled() {
            return Err(FetcherError::Cancelled);
        }

        if next_height > tip {
            // Re-poll the tip in case new blocks landed during the run.
            let refreshed =
                fetch_tip_with_retry(&mut client, &endpoints, &cancel, &mut retry_count).await?;
            if refreshed <= tip {
                break;
            }
            tip = refreshed;
            continue;
        }

        // Compute this batch's [start, end] inclusive range.
        let batch_end = next_height
            .saturating_add(SYNC_BATCH_SIZE.saturating_sub(1))
            .min(tip);

        match download_batch(&mut client, next_height, batch_end).await {
            Ok(blocks) => {
                // watch::Sender::send only fails if all receivers dropped; that's
                // benign — the fetcher should keep filling the cache regardless.
                persist_batch_and_broadcast(&cache_writer, &available_tx, blocks, batch_end)
                    .await?;
                next_height = batch_end.saturating_add(1);
            }
            Err(err) => {
                let msg = err.to_string();
                if !is_transient_transport_error(&msg) {
                    return Err(FetcherError::Transport(msg));
                }
                if retry_count >= MAX_FETCHER_RETRIES {
                    return Err(FetcherError::Transport(format!(
                        "exceeded {MAX_FETCHER_RETRIES} reconnect attempts: {msg}"
                    )));
                }
                retry_count += 1;
                warn!(
                    "fetcher: lightwalletd connection dropped (attempt {retry_count}/{MAX_FETCHER_RETRIES}), reconnecting in {FETCHER_RETRY_DELAY_SECS}s: {msg}"
                );
                if !sleep_or_cancel(&cancel, Duration::from_secs(FETCHER_RETRY_DELAY_SECS)).await {
                    return Err(FetcherError::Cancelled);
                }
                if let Some(new_client) = reconnect(&endpoints).await {
                    client = new_client;
                }
                // Loop continues; same `next_height` retried with fresh client.
            }
        }
    }

    Ok(FetcherSummary {
        final_tip: BlockHeight::from_u32(tip),
        retry_count,
    })
}

/// Persist a downloaded batch and broadcast the new high-water mark.
///
/// Factored out of `run_fetcher` so the cache-write + watch-broadcast contract
/// can be exercised without standing up a mock gRPC client.
async fn persist_batch_and_broadcast(
    cache_writer: &SharedCacheWriter,
    available_tx: &watch::Sender<Option<BlockHeight>>,
    blocks: Vec<zcash_client_backend::proto::compact_formats::CompactBlock>,
    batch_end: u32,
) -> Result<(), FetcherError> {
    if !blocks.is_empty() {
        cache_writer
            .cache()
            .insert(blocks)
            .await
            .map_err(FetcherError::CacheWrite)?;
    }
    let new_height = BlockHeight::from_u32(batch_end);
    let _ = available_tx.send(Some(new_height));
    Ok(())
}

async fn fetch_tip_with_retry(
    client: &mut CompactTxStreamerClient<Channel>,
    endpoints: &str,
    cancel: &CancellationToken,
    retry_count: &mut u32,
) -> Result<u32, FetcherError> {
    loop {
        if cancel.is_cancelled() {
            return Err(FetcherError::Cancelled);
        }
        match client.get_latest_block(ChainSpec::default()).await {
            Ok(resp) => {
                let height = resp.into_inner().height;
                let height_u32 = u32::try_from(height).map_err(|_| {
                    FetcherError::Transport(format!(
                        "lightwalletd returned negative or out-of-range chain tip {height}"
                    ))
                })?;
                return Ok(height_u32);
            }
            Err(err) => {
                let msg = err.to_string();
                if !is_transient_transport_error(&msg) {
                    return Err(FetcherError::Transport(msg));
                }
                if *retry_count >= MAX_FETCHER_RETRIES {
                    return Err(FetcherError::Transport(format!(
                        "exceeded {MAX_FETCHER_RETRIES} reconnect attempts: {msg}"
                    )));
                }
                *retry_count += 1;
                warn!(
                    "fetcher: get_latest_block transport error (attempt {retry_count}/{MAX_FETCHER_RETRIES}): {msg}"
                );
                if !sleep_or_cancel(cancel, Duration::from_secs(FETCHER_RETRY_DELAY_SECS)).await {
                    return Err(FetcherError::Cancelled);
                }
                if let Some(new_client) = reconnect(endpoints).await {
                    *client = new_client;
                }
            }
        }
    }
}

/// Download blocks `[start, end]` (inclusive) via `get_block_range`.
async fn download_batch(
    client: &mut CompactTxStreamerClient<Channel>,
    start: u32,
    end: u32,
) -> Result<Vec<zcash_client_backend::proto::compact_formats::CompactBlock>, tonic::Status> {
    let range = BlockRange {
        start: Some(BlockId {
            height: u64::from(start),
            hash: vec![],
        }),
        end: Some(BlockId {
            height: u64::from(end),
            hash: vec![],
        }),
    };
    let mut stream = client.get_block_range(range).await?.into_inner();
    let mut blocks =
        Vec::with_capacity(usize::try_from(end.saturating_sub(start).saturating_add(1)).unwrap_or(0));
    while let Some(block) = stream.message().await? {
        blocks.push(block);
    }
    Ok(blocks)
}

async fn reconnect(endpoints: &str) -> Option<CompactTxStreamerClient<Channel>> {
    match probe_lightwalletd_endpoints(endpoints).await {
        Ok((client, endpoint, _)) => {
            warn!("fetcher: reconnected to {endpoint}");
            Some(client)
        }
        Err(err) => {
            warn!("fetcher: reconnect probe failed: {err}");
            None
        }
    }
}

/// Sleep for `dur`, returning `false` if cancellation fires first.
async fn sleep_or_cancel(cancel: &CancellationToken, dur: Duration) -> bool {
    // Coarse 100ms granularity is plenty — backoff is measured in seconds.
    let tick = Duration::from_millis(100);
    let mut remaining = dur;
    while remaining > Duration::ZERO {
        if cancel.is_cancelled() {
            return false;
        }
        let step = tick.min(remaining);
        tokio::time::sleep(step).await;
        remaining = remaining.saturating_sub(step);
    }
    !cancel.is_cancelled()
}

/// Same heuristic as `crate::scan::run_wallet_sync_with_retry`. Kept inlined
/// rather than shared to avoid leaking transport-detail strings out of `scan`.
fn is_transient_transport_error(msg: &str) -> bool {
    msg.contains("transport error")
        || msg.contains("h2 protocol error")
        || msg.contains("GoAway")
        || msg.contains("TimedOut")
        || msg.contains("close_notify")
        || msg.contains("UnexpectedEof")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_token_round_trip() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        let clone = token.clone();
        clone.cancel();
        assert!(token.is_cancelled());
        assert!(clone.is_cancelled());
    }

    #[test]
    fn transient_transport_error_taxonomy() {
        // Sample messages drawn from real lightwalletd / tonic transport
        // failures observed during long syncs.
        assert!(is_transient_transport_error(
            "h2 protocol error: error reading a body from connection: connection closed"
        ));
        assert!(is_transient_transport_error(
            "transport error: GoAway, NO_ERROR, library"
        ));
        assert!(is_transient_transport_error(
            "tls close_notify received during handshake"
        ));
        assert!(is_transient_transport_error(
            "io error: connection TimedOut"
        ));
        assert!(is_transient_transport_error(
            "UnexpectedEof while reading response"
        ));

        // Permanent failures should NOT be retried.
        assert!(!is_transient_transport_error("InvalidArgument: malformed request"));
        assert!(!is_transient_transport_error("PermissionDenied"));
        assert!(!is_transient_transport_error("misbehaving server: negative height"));
    }

    #[tokio::test]
    async fn sleep_or_cancel_returns_false_when_cancelled_mid_sleep() {
        let token = CancellationToken::new();
        let token_clone = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token_clone.cancel();
        });
        let completed = sleep_or_cancel(&token, Duration::from_secs(5)).await;
        assert!(!completed);
    }

    #[tokio::test]
    async fn sleep_or_cancel_completes_when_not_cancelled() {
        let token = CancellationToken::new();
        let completed = sleep_or_cancel(&token, Duration::from_millis(50)).await;
        assert!(completed);
    }

    #[tokio::test]
    async fn persist_batch_writes_cache_and_broadcasts_height() {
        use zcash_client_backend::proto::compact_formats::CompactBlock;

        let dir = tempfile::tempdir().unwrap();
        let writer = SharedCacheWriter::open(
            &dir.path().join("cache.sqlite"),
            &dir.path().join("cache.lock"),
        )
        .unwrap();
        let (tx, mut rx) = watch::channel::<Option<BlockHeight>>(None);

        // Synthesize a 3-block batch ending at height 102.
        let blocks: Vec<CompactBlock> = (100u32..=102)
            .map(|h| CompactBlock {
                height: u64::from(h),
                ..Default::default()
            })
            .collect();
        persist_batch_and_broadcast(&writer, &tx, blocks, 102)
            .await
            .expect("persist should succeed");

        // Watch channel reflects the batch end.
        rx.changed().await.expect("watch should fire");
        assert_eq!(*rx.borrow(), Some(BlockHeight::from_u32(102)));

        // Cache contains the rows we inserted.
        assert_eq!(writer.cache().count_blocks().unwrap(), 3);

        // A second batch advances the watermark monotonically.
        let blocks2: Vec<CompactBlock> = (103u32..=105)
            .map(|h| CompactBlock {
                height: u64::from(h),
                ..Default::default()
            })
            .collect();
        persist_batch_and_broadcast(&writer, &tx, blocks2, 105)
            .await
            .unwrap();
        rx.changed().await.expect("watch should fire again");
        assert_eq!(*rx.borrow(), Some(BlockHeight::from_u32(105)));
        assert_eq!(writer.cache().count_blocks().unwrap(), 6);
    }

    #[tokio::test]
    async fn persist_batch_with_empty_blocks_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let writer = SharedCacheWriter::open(
            &dir.path().join("cache.sqlite"),
            &dir.path().join("cache.lock"),
        )
        .unwrap();
        let (tx, rx) = watch::channel::<Option<BlockHeight>>(None);

        persist_batch_and_broadcast(&writer, &tx, vec![], 200)
            .await
            .unwrap();

        // Even with no blocks, we still broadcast the batch_end so receivers
        // can advance past empty ranges (e.g. all-blocks-already-cached).
        assert_eq!(*rx.borrow(), Some(BlockHeight::from_u32(200)));
        assert_eq!(writer.cache().count_blocks().unwrap(), 0);
    }

    #[test]
    fn fetcher_error_display_includes_kind() {
        let err = FetcherError::Cancelled;
        assert_eq!(err.to_string(), "fetcher cancelled");
        let err = FetcherError::Transport("GoAway".to_owned());
        assert!(err.to_string().contains("GoAway"));
    }
}
