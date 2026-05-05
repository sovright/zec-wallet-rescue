# Multi-Seed Scan — Design

**Date:** 2026-05-04
**Status:** Draft, pending implementation plan
**Owners:** Zaki

## Problem

ZECK today scans one ZecWallet Lite seed at a time. Users recovering several old wallets must run the full scan workflow N times in series. The dominant cost in each scan is downloading compact blocks from lightwalletd over a long block range; running N sequential single-seed scans means downloading the same blocks N times. Users want a multi-seed mode where N seeds are recovered in one operation, with the lowest-birthday seed producing results first.

## Goals

- Scan N user-supplied seeds within a single ZECK run, sharing one block-download stream across all of them.
- Earliest-birthday seed begins producing discoveries first. Once the fetcher passes a later seed's birthday, both scanners run concurrently and their discoveries may interleave in real time. The discoveries feed is per-seed within each card and globally append-only at the run level (i.e., the timeline is not strictly birthday-ordered after the catch-up point).
- Single-seed UX is unchanged for users who only have one seed.
- A seed scanned previously as a single-seed run can be added to a multi-seed run and resume from its existing `fully_scanned_height`. Resume requires the *same workspace key*, which today includes `birthday`. To make resume robust against the user re-entering a different birthday on the second run, the resolver looks up any existing workspace by `(data_dir, network, fingerprint)` first; if one exists, its stored birthday is reused and the user-supplied birthday is treated as a no-op (with a UI note "resuming from existing scan at height H").
- CLI parity with the GUI feature.

## Non-goals (v1)

- Per-seed recovery destinations. v1 sweeps everything to a single user-supplied address.
- Per-seed pause/resume granularity. v1 has one "Cancel run" control.
- Cache pruning policy. v1 keeps the shared cache forever; pruning is a follow-up.
- Concurrent multi-seed runs. v1 assumes one active run per network at a time (cache pool is single-writer).

## Architecture

### Today

`zeck_core::scan::run_wallet_sync` (scan.rs:821) couples three responsibilities into one call: download compact blocks via lightwalletd, write them into a per-workspace `SqliteBlockCache`, and scan them into a per-workspace `WalletDb` using `zcash_client_backend::sync::run`. `run_wallet_sync_with_retry` wraps that with GoAway/transport-error reconnect logic.

The block cache and the wallet DB live inside the same workspace directory keyed on `(data_dir, network, seed_fingerprint, birthday, num_accounts | gap_limit)`.

### Change

Split download from scan, and relocate the cache to a network-scoped pool shared across all scans.

**Block fetcher** (one per multi-seed run). Owns the lightwalletd client and the shared `SqliteBlockCache` opened in WAL mode at `data_dir/cache/<network>.sqlite`. Drives downloads from `min(seed.birthday for all seeds, seed.fully_scanned_height + 1 for resumed seeds)` to chain tip in `SYNC_BATCH_SIZE` chunks. Inherits the existing GoAway/transport-error reconnect behaviour from `run_wallet_sync_with_retry`. After each batch is committed to the cache, broadcasts the new available height on a `tokio::sync::watch::Sender<BlockHeight>`.

**Per-seed scanner** (N tasks, one per seed). Each owns a `WalletDb` in its own per-seed workspace under the existing keying scheme. Each subscribes to the fetcher's watch channel. On each tick, the scanner computes `[max(seed.birthday, fully_scanned_height + 1) .. available_tip]` and invokes the lower-level `scan_cached_blocks` API against the shared cache. A scanner whose birthday is above the current available tip simply waits.

Note: scanners do not talk to lightwalletd, so they do not need GoAway/transport-error handling. The reconnect logic from `run_wallet_sync_with_retry` is reused only by the fetcher. Scanner-side errors are restricted to wallet-DB and cache-read failures.

**Cache concurrency.** SQLite WAL mode permits one writer (the fetcher) and many readers (the scanners) safely within a process. The cache file is opened by the fetcher with `BEGIN IMMEDIATE`-style write transactions and by scanners as read-only connections.

**Cross-process exclusion.** A second ZECK process (e.g. CLI run while GUI is open) must not also try to write the cache. Enforced with an OS-level advisory lock on `data_dir/cache/<network>.lock` acquired by the fetcher at run start; if the lock is held, the run fails fast with "another ZECK scan is in progress." Scanners do not need the lock (read-only).

**Cache pruning.** No-op in v1. `BlockCache::truncate` is wired to only run when *all* active scanners have advanced past the truncation height, but the default policy never calls it. The cache grows indefinitely; we accept this for v1 because compact blocks are small and can be wiped manually if needed. Follow-up work can add an explicit "compact cache" maintenance command.

**Single-seed compatibility.** The existing `start_scan` Tauri command and CLI `zeck scan --seed <one>` keep working. Internally they become a degenerate multi-seed run with N=1, transparently picking up the shared cache. Workspaces previously created under the old per-workspace cache layout are migrated lazily: on open, if the old cache exists at the workspace path, its contents are merged into the network-scoped cache and the old file is deleted.

### Key types (zeck-core)

```rust
pub struct MultiSeedRun {
    fetcher: FetcherHandle,
    scanners: Vec<ScannerHandle>,
    cache_path: PathBuf,
    network: Network,
    progress: Arc<Mutex<MultiSeedProgress>>,
}

pub struct MultiSeedProgress {
    // back-compat aggregate (mirrors existing single-seed shape)
    pub blocks_scanned: u64,
    pub synced_to_height: BlockHeight,
    pub discoveries: Vec<ScanDiscovery>, // each carries seed_index
    // new
    pub per_seed: Vec<SeedProgress>,
    pub fetcher: FetcherProgress,
}

pub struct SeedProgress {
    pub index: usize,
    pub fingerprint: SeedFingerprintHex,
    pub label: Option<String>,
    pub birthday: BlockHeight,
    pub fully_scanned_height: BlockHeight,
    pub status: SeedStatus,         // Pending | Scanning | Done | Failed(String)
    pub balance: Option<BalanceSummary>,
}

pub struct FetcherProgress {
    pub downloaded_to_height: BlockHeight,
    pub target_tip: BlockHeight,
    pub retry_count: u32,
}
```

`ScanDiscovery` gains `seed_index: usize` and `seed_fingerprint: String`.

## Data flow

1. **Submit.** User submits N seed entries (`{phrase, birthday | "auto", label}`).
2. **Resolve.** Backend derives each fingerprint, validates phrases, runs `detect_birthday` sequentially for "auto" entries (already O(seconds) thanks to the GetAddressUtxos fast path).
3. **Dedup.** Reject duplicate fingerprints with a row-level error naming the duplicate indexes.
4. **Sort.** Sort the seed list by birthday ascending. This becomes the canonical seed-index order for the run.
5. **Open workspaces.** For each seed, open-or-create its single-seed workspace under the existing keying scheme. If `fully_scanned_height >= seed.birthday` already, the scanner resumes from there.
6. **Open shared cache.** Acquire the network-scoped advisory lock, then open `data_dir/cache/<network>.sqlite` in WAL mode (creating if absent). Migrate any per-workspace caches found in step 5: each old cache is merged into the shared cache sequentially (one seed at a time) and deleted on success. If a single seed's migration fails, log a warning, leave the old cache file in place, and proceed without those blocks — the fetcher will simply re-download them. The run does not abort on migration failure.
7. **Compute fetch start.** `start = min over seeds of max(seed.birthday, seed.fully_scanned_height + 1)`.
8. **Spawn fetcher.** Begins downloading from `start` to chain tip; broadcasts available height on each batch.
9. **Spawn scanners.** Each subscribes to the fetcher and runs its scan loop.
10. **Pump loop** (`commands.rs::start_multi_scan`) emits Tauri progress events on a 250ms cadence, tracking per-scanner discovery cursors so each `ScanDiscovery` is emitted exactly once.
11. **Completion.** Run completes when every scanner reaches the tip, or the user cancels. On cancel, the fetcher signals shutdown via `CancellationToken`, scanners drain their current batch and exit; workspaces remain resumable.

## UI (Tauri GUI)

### Seed entry

The existing single seed-input panel becomes a list. Each row contains:
- Phrase textarea (24 words, validated on blur).
- Birthday: numeric field plus "Auto-detect" button (mirrors the current single-seed control).
- Optional label.
- Remove button (hidden for the only remaining row).

A "+ Add another seed" button appends a row. The form layout is unchanged when only one row exists, so single-seed users see no new UI surface beyond the "+ add another" button.

"Start scan" is disabled until at least one row validates and birthdays for all rows are resolved.

### Scan progress

The current single progress card becomes a stack:
- **Aggregate header**: overall blocks-scanned, fetcher's downloaded-to height, run-level ETA, and reconnect counter.
- **Per-seed cards** (one per seed, in birthday order): label, fingerprint short-hex, birthday, scanned-to height, status pill (Pending / Scanning / Done / Failed).
- **Discoveries feed**: grouped by seed, identical streaming behaviour to today.

### Sweep

After all scanners report Done:
- Header: "X of N seeds funded — Y.YY ZEC total."
- Single destination address field.
- Per-seed summary list (collapsed): each row shows seed label, total, expandable pool breakdown.
- One "Sweep all" button. Sweeps run sequentially per seed; per-seed progress shown in a list. Failures on one seed do not block others.

## CLI

`zeck scan` accepts repeated `--seed`/`--birthday` pairs:

```
zeck scan --seed "phrase one ..." --birthday 2400000 \
          --seed "phrase two ..." --birthday auto
```

Plus a file form:

```
zeck scan --seeds-file ./seeds.txt
```

`seeds.txt` is one entry per line: `phrase` or `phrase | birthday` (birthday optional, defaults to auto).

CLI progress prints a per-seed table updated in place, plus the aggregate fetcher line. CLI sweep mirrors GUI: prompts for one destination, sweeps each funded seed in turn.

## Errors

| Failure | Behaviour |
|---|---|
| Invalid seed at submit | Row-level error; run does not start. |
| Duplicate fingerprint | Row-level error naming the duplicate indexes. |
| Birthday auto-detect failure | Fall back to Sapling activation; warn on the row; allow override before start. |
| Fetcher transport error (GoAway, TLS close_notify, h2 protocol error) | Existing `run_wallet_sync_with_retry` reconnect logic (10 attempts, 5s backoff). Scanners pause; retry counter surfaced in progress. |
| Per-seed scanner error (DB corruption, unexpected wallet state) | Status → `Failed(msg)`. Other scanners continue. Final report lists failed seeds. |
| Shared cache write failure | Fatal to the whole run. Surface error, stop. |
| Cancel | Fetcher signals shutdown; scanners drain current batch and exit. Workspaces remain resumable. |

## Testing

**Unit (zeck-core)**
- Fetcher↔scanner watch-channel coordination against a mock `BlockSource`.
- Scanner correctly waits when its birthday is above the available tip, then catches up.
- Fingerprint dedup rejects duplicates.
- Birthday-ascending sort orders the run deterministically.
- Cache migration: an existing per-workspace cache is merged into the network-scoped cache on open and the old file removed.

**Integration (testnet)**
- Two distinct test seeds with different birthdays: shared cache contains each block exactly once, both wallet DBs reach tip, the earliest-birthday seed's first discovery arrives before the later seed's scanner has produced anything (interleaving thereafter is acceptable).
- Mid-run kill: relaunch resumes both seeds from their respective `fully_scanned_height`.
- Single-seed scan after multi-seed scan: re-scanning seed #1 alone hits the populated shared cache and completes without re-downloading.

**GoAway resilience**
- Extend the existing retry test in scan.rs to assert all scanners stay alive across a fetcher reconnect.

**GUI**
- Hand-tested checklist in TESTING.md: add/remove rows, validation states, start, mid-run cancel, sweep with one funded + one unfunded seed.

## Open questions

- Cache file size in practice on mainnet over multi-year ranges. If it grows uncomfortably large, prioritise the "compact cache" follow-up.
- Whether to surface a "shared cache" indicator in the UI when a scan is benefiting from previously-cached blocks (cosmetic, deferrable).

## Out of scope / follow-ups

- Pruning / compaction of the shared cache.
- Per-seed sweep destinations.
- Concurrent multi-seed runs (would require a cache write lease).
- Streaming-decrypt parallelism within a single seed (orthogonal optimisation).
