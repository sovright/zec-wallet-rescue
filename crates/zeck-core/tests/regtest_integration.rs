//! Integration tests requiring a local regtest Zcash node + lightwalletd.
//!
//! These tests are `#[ignore]` by default — CI doesn't run them because the
//! regtest harness is too heavy to boot per PR. Run locally with:
//!
//! ```bash
//! cargo test --workspace -- --ignored
//! ```
//!
//! after starting a regtest node and pointing lightwalletd at it. The
//! environment variable `ARGOS_REGTEST_LIGHTWALLETD_URL` must be set to
//! the lightwalletd endpoint (e.g. `http://localhost:9067`).
//!
//! ## What this file is
//!
//! Stubs documenting the test surface that the parallel "recovery
//! resilience" PR (this branch) defines but does not fully wire up.
//! Each stub:
//! - has a name matching the `R-*` ID from
//!   `docs/superpowers/test-plans/recovery-resilience.md`
//! - panics with `unimplemented!()` carrying a description of what the
//!   test would verify
//! - is annotated `#[ignore]` so the panic doesn't reach CI
//!
//! A follow-up "regtest harness" PR will replace each `unimplemented!()`
//! body with the actual test, using a shared `RegtestHarness` helper
//! that boots/mines/funds a known seed.
//!
//! ## Why stubs and not just a doc?
//!
//! Two reasons:
//! 1. Discoverable: `cargo test --list -- --ignored` enumerates them,
//!    so the test surface is visible from tooling rather than a doc.
//! 2. Build-checked: the test names compile, so a future rename of an
//!    underlying API surfaces as a build failure on this file rather
//!    than as silent doc drift.

#![allow(clippy::needless_pass_by_value)]

fn regtest_endpoint() -> Option<String> {
    std::env::var("ARGOS_REGTEST_LIGHTWALLETD_URL").ok()
}

/// Ensures the environment is set up; skips with a clear message otherwise.
/// Returns the lightwalletd URL on success.
fn require_regtest() -> String {
    regtest_endpoint().unwrap_or_else(|| {
        // Reaching this means the test was invoked without the harness env
        // var. `#[ignore]` should prevent that in CI; if a human ran
        // --ignored without the setup, surface the problem clearly.
        panic!(
            "ARGOS_REGTEST_LIGHTWALLETD_URL is not set. \
             See crates/zeck-core/tests/regtest_integration.rs module docs \
             for the regtest setup."
        )
    })
}

// ─── R-N8: GoAway frame mid-scan triggers reconnect ─────────────────────────
#[ignore = "requires a regtest node that can be configured to issue HTTP/2 GoAway frames mid-stream"]
#[test]
fn goaway_mid_scan_reconnects_without_duplicate_emissions() {
    let _url = require_regtest();
    // Verify:
    //   - run_wallet_sync_with_retry observes the GoAway, sleeps the
    //     documented 5s, reconnects, and the scan continues from the same
    //     block height.
    //   - ProgressPoller survives the reconnect — the synced_to_height
    //     field continues advancing.
    //   - Discoveries are not re-emitted (dedup via the seen-set in the
    //     pump loop).
    unimplemented!("regtest harness PR will implement");
}

// ─── R-N9: Hostile compact block ────────────────────────────────────────────
#[ignore = "requires a regtest node serving crafted malformed compact blocks"]
#[test]
fn hostile_compact_block_rejected_cleanly() {
    let _url = require_regtest();
    // Verify:
    //   - zcash_client_backend::sync rejects with the expected error variant.
    //   - The wallet DB is not partially corrupted (subsequent clean re-scan
    //     against a non-hostile node succeeds and reaches the same totals
    //     as a fresh-workspace baseline).
    //   - The scan phase ends in Error with a useful message, not a panic.
    unimplemented!("regtest harness PR will implement");
}

// ─── R-N10: All endpoints unreachable ───────────────────────────────────────
#[ignore = "requires the harness to block/firewall the configured endpoints"]
#[test]
fn all_endpoints_unreachable_surfaces_clean_error() {
    let _url = require_regtest();
    // Verify:
    //   - connect_lightwalletd_endpoints exhausts the list within a
    //     bounded number of attempts.
    //   - The error message names "all configured endpoints failed" or
    //     equivalent — not a single endpoint's transport error.
    //   - No silent infinite retry.
    unimplemented!("regtest harness PR will implement");
}

// ─── R-N11: TLS handshake failure ───────────────────────────────────────────
#[ignore = "requires an https endpoint with an expired or self-signed cert"]
#[test]
fn tls_handshake_failure_does_not_fall_back_to_plaintext() {
    let _url = require_regtest();
    // Verify:
    //   - The TLS error is surfaced immediately.
    //   - There is no implicit fall-back to http:// or to a different
    //     trust root.
    //   - The error message names the cert validation failure cause
    //     (expired / unknown CA / hostname mismatch) so the user can act.
    unimplemented!("regtest harness PR will implement");
}

// ─── R-N12: Multi-endpoint fallback ─────────────────────────────────────────
#[ignore = "requires two endpoints, one slow/unresponsive"]
#[test]
fn multi_endpoint_fallback_respects_configured_order() {
    let _url = require_regtest();
    // Verify:
    //   - With "slow,fast" as the endpoint list, the connect loop tries
    //     slow first and falls through to fast within a bounded timeout.
    //   - The "preferred endpoint" reordering surface (preferred argument
    //     to connect_lightwalletd_endpoints) actually changes the order.
    //   - A subsequent reconnect after a GoAway prefers the endpoint that
    //     was successfully serving before the GoAway.
    unimplemented!("regtest harness PR will implement");
}

// ─── R-S25: Sprout-only wallet graceful handling ────────────────────────────
#[ignore = "requires a regtest seed with only Sprout notes (pre-Sapling test fixture)"]
#[test]
fn sprout_only_wallet_scans_cleanly_with_zero_funds() {
    let _url = require_regtest();
    // Verify:
    //   - The scan completes with phase = Complete, not Error.
    //   - Total recovered balance is 0 ZEC.
    //   - No panic anywhere in the librustzcash stack from the absence of
    //     Sapling/Orchard notes.
    //   - The recovery report acknowledges Sprout out-of-scope, not silently.
    unimplemented!("regtest harness PR will implement");
}

// ─── R-S26: Reorg during scan ───────────────────────────────────────────────
#[ignore = "requires regtest to mine a reorg via invalidateblock / reconsiderblock"]
#[test]
fn reorg_during_scan_invalidates_and_rescans_affected_range() {
    let _url = require_regtest();
    // Verify:
    //   - The scan detects the reorg via zcash_client_backend's chain
    //     reconciliation.
    //   - The wallet DB rolls back to the common ancestor and re-scans.
    //   - Final balance after re-scan matches the post-reorg ground truth
    //     (not the pre-reorg snapshot).
    unimplemented!("regtest harness PR will implement");
}

// ─── R-S27: Crash mid-scan resume ───────────────────────────────────────────
#[ignore = "requires terminating the scan process partway through"]
#[test]
fn crash_mid_scan_resumes_from_fully_scanned_height() {
    let _url = require_regtest();
    // Verify:
    //   - SIGKILL the argos-cli process partway through a scan.
    //   - Restart with identical args; the second run picks up at the
    //     persisted fully_scanned_height, not from birthday.
    //   - Final balance matches the baseline of an uninterrupted scan
    //     against the same seed.
    unimplemented!("regtest harness PR will implement");
}

// ─── R-S29: Crash mid-broadcast ─────────────────────────────────────────────
#[ignore = "requires interrupting between two per-account sweep broadcasts"]
#[test]
fn crash_mid_broadcast_does_not_double_spend_on_resume() {
    let _url = require_regtest();
    // Verify:
    //   - SIGKILL after the first per-account sweep tx has broadcast but
    //     before the second.
    //   - Restart; the wallet DB sees the broadcast tx after sync.
    //   - The resume sweep does NOT re-broadcast the same tx (no
    //     double-spend attempt).
    //   - The remaining accounts sweep normally.
    unimplemented!("regtest harness PR will implement");
}

// ─── R-W24: Two argos instances on same workspace ───────────────────────────
#[ignore = "requires launching two processes against the same workspace dir"]
#[test]
fn two_instances_same_workspace_cancels_first() {
    let _url = require_regtest();
    // Verify:
    //   - Launch argos-cli #1 against workspace X; let it start scanning.
    //   - Launch argos-cli #2 against the same workspace X.
    //   - #1 is cancelled (per the conflict-cancellation logic in
    //     RecoveryService::start_scan).
    //   - #2 proceeds without SQLite lock errors.
    //   - The combined state of #2 reflects only its own scan, not a
    //     half-merge from #1.
    unimplemented!("regtest harness PR will implement");
}

// ─── R-W25: Workspace deleted between scan and sweep ───────────────────────
#[ignore = "requires deleting the workspace dir mid-flow"]
#[test]
fn workspace_deleted_between_scan_and_sweep_surfaces_clean_error() {
    let _url = require_regtest();
    // Verify:
    //   - Complete a scan; reach the sweep proposal screen.
    //   - Delete the workspace directory externally (rm -rf).
    //   - The sweep proposal/execute call fails with a clear "workspace
    //     gone" message, not a SQLite open error or panic.
    unimplemented!("regtest harness PR will implement");
}

// ─── R-W26: Workspace permissions tampered ─────────────────────────────────
#[cfg(unix)]
#[ignore = "requires chmod-ing the workspace dir to read-only mid-flow"]
#[test]
fn workspace_permissions_tampered_surfaces_clean_error() {
    let _url = require_regtest();
    // Verify:
    //   - chmod 0o444 the workspace dir mid-scan.
    //   - The next write attempt fails with a clear "cannot write to
    //     workspace" message, not a silent partial-state corruption.
    //   - Restoring permissions and resuming the scan recovers cleanly.
    unimplemented!("regtest harness PR will implement");
}
