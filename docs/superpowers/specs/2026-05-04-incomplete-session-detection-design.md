# Incomplete Sync Session Detection — Design

**Date:** 2026-05-04
**Scope:** GUI only (Tauri app). CLI is out of scope for this spec.

## Problem

ZECK's scan can run for a long time and may be interrupted (app close, crash, OS sleep, network drop beyond retry budget). Today, resuming requires the user to remember the exact seed, birthday, and account/gap settings used previously and re-enter them; auto-resume happens silently inside the existing workspace path keying. There is no surface in the GUI that tells the user "you have unfinished scans on this machine — pick one to continue."

This spec adds a launch-time UI in the GUI that lists incomplete scan sessions found on disk and lets the user resume one after providing the matching seed.

## Goals

- On GUI launch, detect any workspaces under the configured data dir with an incomplete scan and present them to the user before the new-scan form.
- Allow the user to identify their own scans without seeing the seed (via a label they set when starting the scan).
- Require the matching seed to actually resume — picking a row in the list does not by itself unlock anything.
- Preserve current behavior when no incomplete sessions exist (user goes straight to the new-scan form).

## Non-Goals

- CLI parity (`--list-sessions`, `--resume`). Out of scope; can be added later.
- Surfacing scan history for *completed* sessions.
- Storing or recovering the seed itself.
- Cross-machine session discovery.

## Architecture

A small JSON sidecar (`session.json`) lives alongside each workspace's `WalletDb`. It is written when a scan starts and updated as the scan progresses; the `completed` flag flips to `true` only when sync reaches the chain tip recorded as `target_height`.

On GUI launch, a new Tauri command walks the data dir, reads each sidecar plus `fully_scanned_height` from the corresponding `WalletDb` (which does not require the seed), and returns rows for any workspace where `completed == false`. The user picks a row → the GUI prompts for the seed → the backend re-derives the seed fingerprint, verifies it matches the workspace's path segment, and only then hands off to the existing scan entry point. The existing resume logic in `scan.rs` (which keys on `(network, seed_fingerprint, birthday, accounts/gap)`) picks up from `fully_scanned_height` automatically.

The seed never leaves the backend. The sidecar contains no seed material.

## Components

### `crates/zeck-core/src/workspace.rs`

New types and functions:

- `SessionMetadata` (serde):
  - `label: String`
  - `network: Network` (mainnet/testnet)
  - `birthday: BlockHeight`
  - `target_height: Option<BlockHeight>` — chain tip recorded at start of most recent run; `None` for legacy/missing
  - `last_run_at: DateTime<Utc>`
  - `completed: bool`
  - `schema_version: u32` — starts at 1
- `write_session_metadata(workspace: &WorkspacePath, meta: &SessionMetadata) -> Result<()>` — atomic write (temp file + rename).
- `read_session_metadata(workspace: &WorkspacePath) -> Result<Option<SessionMetadata>>` — `Ok(None)` if file missing; `Err` on I/O; logs and returns `Ok(None)` on parse failure (treated as legacy).
- `list_incomplete_sessions(data_dir: &Path) -> Result<Vec<IncompleteSession>>` — walks `<data_dir>/<network>/<fingerprint>/<birthday>/<accounts>/`, opens each `WalletDb` read-only to read `fully_scanned_height`, joins with the sidecar, and returns rows where `completed == false` OR sidecar is missing/legacy.
- `IncompleteSession`:
  - `workspace_path: PathBuf`
  - `label: String` — `"(unlabeled scan)"` for legacy
  - `network: Network`
  - `birthday: BlockHeight`
  - `synced_to_height: Option<BlockHeight>` — from DB
  - `target_height: Option<BlockHeight>` — from sidecar
  - `last_run_at: Option<DateTime<Utc>>`
- `verify_seed_for_workspace(workspace_path: &Path, seed_phrase: &str) -> Result<()>` — re-derives the seed fingerprint and compares to the path segment; returns `Err(SeedMismatch)` on mismatch.

### `crates/zeck-core/src/scan.rs`

- At scan start (after the workspace is materialized, before the sync loop), write `session.json` with `completed: false`, the user-provided `label`, current `target_height` (probed from any healthy lightwalletd endpoint), and `last_run_at = now()`.
- At successful completion (sync loop confirms `fully_scanned_height >= target_height`), update the sidecar with `completed: true`.
- On each retry within `run_wallet_sync_with_retry`, update `last_run_at` opportunistically — best-effort, do not fail the scan if the write errors.
- A scan that exits via error or interruption leaves `completed: false`. That is the signal the launch-time list keys on.

### `gui/src-tauri/src/commands.rs`

Two new commands:

- `list_incomplete_sessions(data_dir: Option<String>) -> Result<Vec<SessionRow>, String>` — calls `workspace::list_incomplete_sessions`. Resolves data dir via existing `default_data_dir` if `None`.
- `resume_session(workspace_path: String, seed_phrase: String, label: Option<String>) -> Result<(), String>` — calls `verify_seed_for_workspace`, then enters the existing `start_scan` code path with parameters reconstructed from the workspace path. Optional `label` parameter lets the user rename the session at resume time; if provided, the sidecar is rewritten before the scan starts.

`SessionRow` mirrors `IncompleteSession` with serde-friendly types (heights as `u32`, paths as `String`, timestamps as ISO-8601).

The existing `start_scan` command gains a `label: String` argument (or accepts a `label` field on its existing config struct) so the sidecar can be written with the user's label.

### `gui/src/main.js` + `index.html` + `styles.css`

- New `#incomplete-sessions-panel` rendered on launch when `list_incomplete_sessions` returns a non-empty list. Layout: heading "Resume an unfinished scan", a list of rows, and a "Start a new scan instead" button at the bottom.
- Each row shows:
  - Label (bold)
  - `<network>` · `birthday <b>` · scanned `<X>` of `<Y>` (or `<X>` of `?` for legacy)
  - "last run <relative time>"
  - "Resume" button
- Resume click → modal/overlay with seed phrase input only (no birthday or account inputs — those come from the workspace path). Submit → call `resume_session`. On `SeedMismatch`, show inline error "This seed doesn't match this scan." and stay on the modal.
- "Start a new scan instead" reveals the existing new-scan form, which gains an inline **Label** text field (default `"Scan started <YYYY-MM-DD>"`) above the seed input.

## Data Flow

**Launch:**
1. Frontend calls `list_incomplete_sessions`.
2. If empty → render the existing new-scan form.
3. If non-empty → render the sessions panel.

**Resume:**
1. User clicks Resume on a row → seed-entry modal opens.
2. User submits seed → `resume_session(workspace_path, seed_phrase)`.
3. Backend calls `verify_seed_for_workspace`. On mismatch: return error → frontend shows inline error.
4. On match: backend reconstructs `(network, birthday, accounts/gap)` from the workspace path, re-uses the existing `start_scan` flow, and the sync loop resumes from `fully_scanned_height`.

**New scan:**
1. User fills out the form, including the **Label** field.
2. Backend creates the workspace (existing path), writes `session.json` with `completed: false, label, target_height, last_run_at`.
3. Sync runs. On success, sidecar flips to `completed: true`. On failure/interruption, sidecar stays `completed: false`.

## Error Handling and Edge Cases

| Case | Behavior |
|---|---|
| Workspace dir exists but no `session.json` (legacy) | Listed with `label = "(unlabeled scan)"`, `target_height = None`. User can resume. After successful resume, a sidecar is written. |
| Corrupt `session.json` | Logged at warn level; treated as legacy. |
| Workspace dir exists but no `WalletDb` | Skipped (not listable). |
| Two workspaces with the same fingerprint but different `(birthday, accounts/gap)` | Each shown as a separate row — that is the existing keying invariant. |
| Seed fingerprint mismatch on resume | Clear inline error in the modal; user can retry. No backend state changes. |
| User clicks "Start a new scan instead" while incomplete sessions exist | Allowed. New scan creates its own workspace. The old incomplete sessions remain listed on next launch. |
| `target_height` was not knowable at scan start (all lightwalletd endpoints unreachable) | Sidecar is still written with `target_height = None`; the row shows "scanned X of ?". `completed` flips when sync's own internal "reached tip" condition fires. |
| `data_dir` does not exist yet (first launch ever) | `list_incomplete_sessions` returns empty list. |
| User edits `data_dir` in settings between launches | List reflects the new dir. Old workspaces under the previous dir are not surfaced (matches today's behavior). |

## Testing

**Unit (`workspace.rs`):**
- Sidecar round-trip: write → read → equal.
- `list_incomplete_sessions` with a fixture data dir containing: one complete workspace, one incomplete with sidecar, one legacy (no sidecar), one corrupt sidecar, one workspace dir with no DB. Assert the returned set and ordering.
- `verify_seed_for_workspace` happy path and mismatch path.
- Atomic write: verify partial writes leave the previous sidecar intact (simulate by writing through a wrapper that fails mid-stream).

**Integration (`scan.rs`):**
- Run a short sync against a mock or testnet endpoint; assert sidecar transitions `completed: false → true` only after `fully_scanned_height >= target_height`.
- Simulate interruption (drop the future) mid-sync; assert sidecar remains `completed: false`.

**GUI manual tests:**
- Launch with no sessions → new-scan form renders directly.
- Launch with one incomplete session → panel renders with correct label, network, progress, last-run time.
- Click Resume → seed modal → wrong seed → inline error → correct seed → scan resumes from previous block.
- Launch with a legacy workspace (no sidecar) → row shows "(unlabeled scan)" and "scanned X of ?".
- Start a new scan with the label field → after a few seconds, kill the app → relaunch → session shows up with the right label.

## Open Items

None blocking. Future work (out of scope here): CLI parity, deletion / archival of stale incomplete sessions from the GUI, surfacing completed sessions for re-sweep.
