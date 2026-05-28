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

// Gate the whole file behind the `argos-network` feature. Without it, Argos
// can't talk to a regtest-style local chain (validate_lightwalletd_network
// rejects the regtest chain name and Sapling activation height), so the C2
// tests are guaranteed to fail at scan-start. Compiling them out under the
// default feature set keeps `cargo test --workspace -- --ignored` clean for
// contributors who haven't booted the harness; opt in with
// `cargo test --features argos-network -- --ignored` after running the
// harness setup in tests/regtest/.
#![cfg(feature = "argos-network")]
#![allow(clippy::needless_pass_by_value)]

// Shared harness module — see `tests/common/regtest_harness.rs` for the
// `RegtestHarness` fixture and its env-var contract. `#[allow(dead_code)]`
// because not every helper in `common::regtest_harness` is consumed by
// every test in this file; cargo's per-binary unused-warning policy would
// otherwise complain about the module's other items.
#[allow(dead_code)]
mod common;
use common::regtest_harness::RegtestHarness;

use std::path::PathBuf;
use std::time::Duration;

use argos_core::{
    derive_accounts, workspace::RecoveryWorkspace, RecoveryService, RuntimeScanConfig, ScanConfig,
    ScanHandle, ScanPhase, SweepRequest, ZeckNetwork,
};
use secrecy::SecretString;

// ─── Shared setup helper ─────────────────────────────────────────────────────

/// Boot a scan against the Argos network harness with the canonical test
/// seed, poll until completion, and hand back everything a workspace-level
/// test needs to attack the workspace.
///
/// The returned `temp_data_dir` is kept by the caller so its `Drop` doesn't
/// run before the test body finishes — `tempfile::TempDir` removes the
/// directory tree on drop.
async fn complete_scan_against_test_seed(
    harness: &RegtestHarness,
    temp_data_dir: &tempfile::TempDir,
    label: &str,
) -> ScannedFixture {
    // Build the runtime config first so we can compute the workspace path
    // deterministically without involving the service. RecoveryWorkspace's
    // path is a hash of (network, seed, birthday, scope); identical args
    // to `start_scan` produce the same root.
    let runtime = RuntimeScanConfig {
        seed_phrase: SecretString::new(harness.test_seed().to_owned()),
        // The Argos network activates Sapling at height 1; setting a tiny
        // birthday keeps the scan fast on regtest. zcashd-regtest tops out
        // at ~200 blocks after setup.sh runs, so the scan is sub-second.
        birthday: 1,
        num_accounts: Some(2),
        gap_limit: 5,
        lightwalletd_url: harness.lightwalletd_url().to_owned(),
        data_dir: temp_data_dir.path().to_path_buf(),
        network: ZeckNetwork::Testnet,
        label: label.to_owned(),
    };
    let workspace_root = RecoveryWorkspace::from_runtime(&runtime)
        .expect("compute workspace path from runtime config")
        .root()
        .to_path_buf();

    // Derive account 1's UA for the sweep destination. We never broadcast,
    // and propose_sweep doesn't care if source == destination — using a
    // derived address from the same seed avoids needing a separately-funded
    // second wallet in the harness.
    let accounts = derive_accounts(&runtime.seed_phrase, runtime.network, 2)
        .expect("derive_accounts for destination UA");
    let destination_ua = accounts[1].unified_address.clone();

    let scan_config = ScanConfig {
        birthday: runtime.birthday,
        num_accounts: runtime.num_accounts,
        gap_limit: runtime.gap_limit,
        lightwalletd_url: runtime.lightwalletd_url.clone(),
        data_dir: runtime.data_dir.clone(),
        network: runtime.network,
        label: runtime.label.clone(),
    };

    let service = RecoveryService::new();
    let handle = service
        .start_scan(
            scan_config,
            SecretString::new(harness.test_seed().to_owned()),
        )
        .await
        .expect("start_scan against argos-network harness");

    // Bounded poll — regtest scans usually complete in under a second from
    // birthday=1 to ~200 blocks. 120s is generous headroom for cold disks.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let progress = service
            .get_scan_progress(&handle)
            .await
            .expect("get_scan_progress");
        match progress.phase {
            ScanPhase::Complete => break,
            ScanPhase::Error => {
                panic!("[regtest] scan errored: {:?}", progress.error);
            }
            ScanPhase::Cancelled => {
                panic!("[regtest] scan unexpectedly cancelled mid-poll")
            }
            _ => {
                if std::time::Instant::now() > deadline {
                    panic!(
                        "[regtest] scan did not complete within 120s; last phase = {:?}",
                        progress.phase
                    );
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }

    assert!(
        workspace_root.exists(),
        "[regtest] workspace root {} does not exist after scan completes",
        workspace_root.display()
    );

    ScannedFixture {
        service,
        handle,
        workspace_root,
        destination_ua,
    }
}

/// Bundle of the post-scan state the workspace-level integration tests need.
struct ScannedFixture {
    service: RecoveryService,
    handle: ScanHandle,
    workspace_root: PathBuf,
    destination_ua: String,
}

// ─── R-N8: GoAway frame mid-scan triggers reconnect ─────────────────────────
#[ignore = "requires a regtest node that can be configured to issue HTTP/2 GoAway frames mid-stream"]
#[test]
fn goaway_mid_scan_reconnects_without_duplicate_emissions() {
    let _harness = RegtestHarness::require();
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
    let _harness = RegtestHarness::require();
    // Verify:
    //   - zcash_client_backend::sync rejects with the expected error variant.
    //   - The wallet DB is not partially corrupted (subsequent clean re-scan
    //     against a non-hostile node succeeds and reaches the same totals
    //     as a fresh-workspace baseline).
    //   - The scan phase ends in Error with a useful message, not a panic.
    unimplemented!("regtest harness PR will implement");
}

// ─── R-N10: All endpoints unreachable ───────────────────────────────────────
#[ignore = "requires the Argos network harness (tests/regtest/ booted, ARGOS_REGTEST_LIGHTWALLETD_URL exported)"]
#[tokio::test]
async fn all_endpoints_unreachable_surfaces_clean_error() {
    // Verifies the failure mode when every configured lightwalletd endpoint
    // refuses the connection. Three properties:
    //
    //   1. `connect_lightwalletd_endpoints` exhausts the list within a
    //      bounded timeout — no silent infinite retry. Enforced via a
    //      `tokio::time::timeout` wrapper as a defensive check on top of
    //      the function's own per-endpoint connect semantics.
    //
    //   2. The returned error is the aggregated "all endpoints failed"
    //      variant, not a single endpoint's transport error. Users with
    //      multi-endpoint configurations need to know that *every* fallback
    //      was tried before giving up, not just the first one.
    //
    //   3. The error names each failing endpoint so it's actionable. The
    //      error string contains both endpoint URLs (the validator accepts
    //      them; only the TCP connect refuses), enabling the GUI/CLI to
    //      surface "tried these N, none worked" rather than a vague
    //      "couldn't connect."
    //
    // Does not actually use the harness URL — but the harness env var
    // gate via `RegtestHarness::require()` ensures the test only runs as
    // part of the C2 integration suite (when someone explicitly booted the
    // setup), not as an accidental unit test.

    let _harness = RegtestHarness::require();

    // Two unreachable URLs on different ports. Both pass the loopback +
    // valid-port URL validator; both will fail TCP connect with
    // ECONNREFUSED in well under a second.
    let combined = "http://127.0.0.1:1,http://127.0.0.1:2";

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        argos_core::lightwalletd::connect_lightwalletd_endpoints(combined, None),
    )
    .await
    .expect(
        "[regtest] connect_lightwalletd_endpoints must return within 10s; \
         no silent infinite retry permitted",
    );

    let err = outcome.expect_err(
        "[regtest] all-unreachable list must surface Err, not Ok",
    );

    let msg = err.to_string();
    assert!(
        msg.contains("failed to connect to any"),
        "[regtest] expected aggregated 'failed to connect to any' wording so \
         the GUI can render 'every endpoint failed' rather than a single \
         transport error; got: {msg}"
    );
    assert!(
        msg.contains("127.0.0.1:1") && msg.contains("127.0.0.1:2"),
        "[regtest] expected the error message to name both attempted endpoints \
         (so the user can see what was tried); got: {msg}"
    );

    eprintln!("[regtest] all-unreachable failed as expected: {err}");
}

// ─── R-N11: TLS handshake failure ───────────────────────────────────────────
#[ignore = "requires the Argos network harness (tests/regtest/ booted, ARGOS_REGTEST_LIGHTWALLETD_URL exported)"]
#[tokio::test]
async fn tls_handshake_failure_does_not_fall_back_to_plaintext() {
    // Verifies that an `https://` endpoint whose TLS handshake fails is
    // surfaced as Err rather than silently falling back to plaintext.
    //
    // Three properties exercised:
    //   1. The result is Err. `connect_lightwalletd_endpoints` does not
    //      have any implicit plaintext-fallback code path; this test
    //      pins that fact against future regressions.
    //   2. The Err is delivered within a bounded timeout (15s wrapper).
    //      No silent indefinite retry.
    //   3. The failure mode is distinguishable from a TCP-level
    //      "connection refused" (which would indicate the listener
    //      crashed before the client connected — a different bug class).
    //      The assertion is structural: the error string must NOT
    //      contain "connection refused".
    //
    // The cert-validation-cause property from the original stub (the error
    // names "expired" / "unknown CA" / "hostname mismatch") is deferred:
    // it requires a server that actually performs a TLS handshake with a
    // specific bad cert, which means generating + spinning up a TLS
    // listener with a self-signed identity. That belongs in a follow-up
    // PR once we have a cert-fixture helper; this PR uses the simpler
    // "TCP accepts but no TLS frames" simulator below.

    let _harness = RegtestHarness::require();

    // Spawn a TCP listener that accepts connections but never sends any
    // TLS frames. tonic's TLS client times out / errors when no
    // ServerHello arrives. tokio's "net" feature is enabled transitively
    // via tonic's transport stack, so `TcpListener` is reachable from
    // this integration test without us adding tokio features explicitly.
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("[regtest] bind random loopback port for TLS-failure simulation");
    let port = listener
        .local_addr()
        .expect("[regtest] read local_addr of TLS-failure listener")
        .port();

    // Background accept loop. Each connection is drained for a short read
    // (the client's ClientHello) and then dropped without any response,
    // which surfaces to tonic as a TLS handshake failure.
    let _accept_task = tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(c) => c,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                // sock drops at end of scope → connection closes →
                // client's TLS handshake fails with unexpected EOF.
            });
        }
    });

    let url = format!("https://localhost:{port}");

    let outcome = tokio::time::timeout(
        Duration::from_secs(15),
        argos_core::lightwalletd::connect_lightwalletd_endpoints(&url, None),
    )
    .await
    .expect(
        "[regtest] connect_lightwalletd_endpoints must return within 15s; \
         no silent indefinite TLS retry permitted",
    );

    let err = outcome.expect_err(
        "[regtest] https endpoint with a non-TLS listener must surface Err, \
         not Ok (no plaintext fallback)",
    );

    let msg_lower = err.to_string().to_ascii_lowercase();
    assert!(
        !msg_lower.contains("connection refused"),
        "[regtest] expected a TLS-handshake / transport error, not \
         'connection refused' (which would indicate the listener died \
         before the client connected): {err}"
    );

    eprintln!("[regtest] TLS handshake failure as expected: {err}");
}

// ─── R-N12: Multi-endpoint fallback ─────────────────────────────────────────
#[ignore = "requires the Argos network harness (tests/regtest/ booted, ARGOS_REGTEST_LIGHTWALLETD_URL exported)"]
#[tokio::test]
async fn multi_endpoint_fallback_respects_configured_order() {
    // Verifies the comma-separated-endpoints + fallback contract that the
    // GUI exposes as the "lightwalletd URLs" field. Two properties:
    //
    //   1. When the first endpoint in the list is unreachable, the connect
    //      loop falls through to the second within a bounded timeout and
    //      returns the second's URL as the established endpoint.
    //
    //   2. The `preferred` argument to `connect_lightwalletd_endpoints`
    //      reorders the list — passing the healthy harness URL as
    //      `preferred` makes it tried first, even when it's listed second
    //      in the raw comma-separated input.
    //
    // The "subsequent reconnect after a GoAway prefers the previously-
    // serving endpoint" sub-property from the original stub description is
    // deferred — it requires server-side GoAway injection (custom
    // lightwalletd build or sidecar proxy) and belongs in the R-N8 stub
    // when that lands.

    let harness = RegtestHarness::require();
    let harness_url = harness.lightwalletd_url().to_owned();

    // `http://127.0.0.1:1` is the canonical "nothing listening" URL on
    // loopback. The validator accepts it (port 1 is a valid port; loopback
    // hosts allow plaintext http per Argos's lightwalletd contract), but
    // the TCP connect attempt will fail with ECONNREFUSED in well under a
    // second on every supported platform.
    const UNREACHABLE: &str = "http://127.0.0.1:1";

    // ── Property 1: fallback after the first endpoint fails. ────────────
    let combined = format!("{UNREACHABLE},{harness_url}");
    let (_client, established) =
        argos_core::lightwalletd::connect_lightwalletd_endpoints(&combined, None)
            .await
            .expect("connect_lightwalletd_endpoints must fall back to the harness URL");
    assert_eq!(
        established, harness_url,
        "[regtest] expected fallback to {harness_url}, got {established}"
    );

    // ── Property 2: `preferred` reorders the list. ──────────────────────
    // Same combined URL — harness still appears second — but the preferred
    // argument names it explicitly, which must reorder it to the front.
    let (_client, established) = argos_core::lightwalletd::connect_lightwalletd_endpoints(
        &combined,
        Some(&harness_url),
    )
    .await
    .expect("connect with preferred=harness must succeed on the first attempt");
    assert_eq!(
        established, harness_url,
        "[regtest] preferred reordering should have surfaced harness first; got {established}"
    );
}

// ─── R-S25: Sprout-only wallet graceful handling ────────────────────────────
#[ignore = "requires a regtest seed with only Sprout notes (pre-Sapling test fixture)"]
#[test]
fn sprout_only_wallet_scans_cleanly_with_zero_funds() {
    let _harness = RegtestHarness::require();
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
    let _harness = RegtestHarness::require();
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
    let _harness = RegtestHarness::require();
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
    let _harness = RegtestHarness::require();
    // Verify:
    //   - SIGKILL after the first per-account sweep tx has broadcast but
    //     before the second.
    //   - Restart; the wallet DB sees the broadcast tx after sync.
    //   - The resume sweep does NOT re-broadcast the same tx (no
    //     double-spend attempt).
    //   - The remaining accounts sweep normally.
    unimplemented!("regtest harness PR will implement");
}

// ─── R-W24: Two scans against the same workspace cancels the first ─────────
#[ignore = "requires the Argos network harness (tests/regtest/ booted, ARGOS_REGTEST_LIGHTWALLETD_URL exported)"]
#[tokio::test]
async fn two_instances_same_workspace_cancels_first() {
    // Verifies the in-process conflict-cancellation logic in
    // `RecoveryService::start_scan`: when a second `start_scan` is issued
    // against a config that resolves to the same workspace as a previously
    // active scan, the existing session is cancelled before the new one
    // proceeds. This is the property that protects the GUI's typical
    // "double-click Start Scan" race.
    //
    // ## What this test covers
    //
    //   1. The second `start_scan` returns a fresh handle without
    //      blocking on or merging with the first.
    //   2. After the second `start_scan` returns, the first handle's
    //      session has been cancelled (phase = Cancelled).
    //   3. The second scan proceeds to ScanPhase::Complete — workspace
    //      reuse after cancellation does not produce SQLite lock errors
    //      or half-merged state. Final balances on the second handle
    //      reflect a complete scan, not a partial one.
    //
    // ## What this test deliberately does NOT cover
    //
    // Two argos-cli *subprocesses* against the same workspace would
    // exercise SQLite WAL contention, not Argos's cancellation logic
    // (each subprocess has its own RecoveryService, so the in-process
    // cancellation path doesn't fire across processes). That belongs in a
    // separate test with subprocess scaffolding, which lands with the
    // R-S27/R-S29 SIGKILL work.

    let harness = RegtestHarness::require();
    let temp_data_dir = tempfile::tempdir().expect("tempdir");

    let scan_config = ScanConfig {
        birthday: 1,
        num_accounts: Some(2),
        gap_limit: 5,
        lightwalletd_url: harness.lightwalletd_url().to_owned(),
        data_dir: temp_data_dir.path().to_path_buf(),
        network: ZeckNetwork::Testnet,
        // Labels go into session.json — the workspace path itself is
        // derived from (seed, network, birthday, gap-strategy) only, so
        // changing the label does NOT change the workspace identity. The
        // conflict-cancellation logic will fire even with different
        // labels, which is the correct behaviour (a relaunched session
        // with a different label is still the same workspace).
        label: "argos-rw24-first".to_owned(),
    };

    let service = RecoveryService::new();

    let handle1 = service
        .start_scan(
            scan_config.clone(),
            SecretString::new(harness.test_seed().to_owned()),
        )
        .await
        .expect("first start_scan must succeed");

    // Hand off briefly so the spawned scan task gets at least one tick.
    // Cancellation works regardless of phase (it sets the flag + aborts
    // the task handle even mid-Idle), but giving the first scan a chance
    // to actually begin makes the "we really did cancel something in
    // flight" property meaningful.
    tokio::task::yield_now().await;

    let handle2 = service
        .start_scan(
            ScanConfig {
                label: "argos-rw24-second".to_owned(),
                ..scan_config
            },
            SecretString::new(harness.test_seed().to_owned()),
        )
        .await
        .expect("second start_scan must succeed against the same workspace");

    assert_ne!(
        handle1.id, handle2.id,
        "[regtest] start_scan must return a fresh handle, not merge with the first"
    );

    // The first handle's session must be Cancelled. cancel_scan sets the
    // phase synchronously before returning, then aborts the task handle.
    // Either outcome (still in the sessions map as Cancelled, or already
    // cleaned up via SESSION_RETENTION_SECS) is acceptable — but a
    // still-Running phase would be a real bug.
    match service.get_scan_progress(&handle1).await {
        Ok(progress) => {
            assert_eq!(
                progress.phase,
                ScanPhase::Cancelled,
                "[regtest] first session must be Cancelled after the second \
                 start_scan; got phase = {:?}",
                progress.phase,
            );
        }
        Err(_) => {
            // Session retention cleanup ran ahead of us; the first handle
            // is no longer in the map. Acceptable — the property under
            // test is "the first scan stopped," which is necessarily true
            // if the handle is gone.
        }
    }

    // Second scan must run to completion. 120s is generous headroom for a
    // ~200-block regtest scan.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let progress = service
            .get_scan_progress(&handle2)
            .await
            .expect("get_scan_progress on the surviving handle");
        match progress.phase {
            ScanPhase::Complete => break,
            ScanPhase::Error => {
                panic!("[regtest] second scan errored: {:?}", progress.error)
            }
            ScanPhase::Cancelled => {
                panic!(
                    "[regtest] second scan was unexpectedly cancelled — \
                     the conflict-cancellation logic should target the \
                     PRIOR scan, not the new one"
                )
            }
            _ => {
                if std::time::Instant::now() > deadline {
                    panic!(
                        "[regtest] second scan did not complete within 120s; \
                         last phase = {:?}",
                        progress.phase
                    );
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }

    eprintln!("[regtest] first scan cancelled, second scan ran to completion");
}

// ─── R-W25: Workspace deleted between scan and sweep ───────────────────────
#[ignore = "requires the Argos network harness (tests/regtest/ booted, ARGOS_REGTEST_LIGHTWALLETD_URL exported)"]
#[tokio::test]
async fn workspace_deleted_between_scan_and_sweep_surfaces_clean_error() {
    // Verifies that an externally-deleted workspace surfaces a clean Err
    // from `propose_sweep`, not a panic or partial-state corruption.
    //
    // Scenario: user completes a scan in the GUI, then `rm -rf`s the
    // workspace directory from another terminal before clicking Sweep.
    // Argos must surface this as a user-actionable error rather than
    // crashing or silently producing an empty proposal.

    let harness = RegtestHarness::require();
    let temp_data_dir = tempfile::tempdir().expect("tempdir for workspace");
    let fixture = complete_scan_against_test_seed(&harness, &temp_data_dir, "argos-rw25").await;

    // Simulate the external rm -rf.
    std::fs::remove_dir_all(&fixture.workspace_root)
        .expect("remove_dir_all on the workspace root must succeed");
    assert!(
        !fixture.workspace_root.exists(),
        "[regtest] workspace root should be gone after remove_dir_all"
    );

    // The sweep request itself is valid — destination is a real UA and the
    // rate fields are absent. The only thing different from a normal sweep
    // is that the workspace under the service's recorded handle is gone.
    let request = SweepRequest {
        destination: fixture.destination_ua.clone(),
        memo: None,
        max_fee_zatoshis: None,
        donation_rate: None,
        donor_email: None,
    };

    let result = fixture.service.propose_sweep(&fixture.handle, request).await;
    let err = result.expect_err(
        "propose_sweep against a deleted workspace must return Err, not Ok",
    );

    // Don't pin the error variant — the wallet-DB / cache-DB / sidecar-JSON
    // layers all touch the workspace and any of them surfacing the missing
    // path first is correct. The contract is: a clean Err that the GUI/CLI
    // can render to a user, not a panic.
    eprintln!("[regtest] propose_sweep failed as expected after workspace deletion: {err}");
}

// ─── R-W26: Workspace permissions tampered ─────────────────────────────────
#[cfg(unix)]
#[ignore = "requires the Argos network harness (tests/regtest/ booted, ARGOS_REGTEST_LIGHTWALLETD_URL exported)"]
#[tokio::test]
async fn workspace_permissions_tampered_surfaces_clean_error() {
    // Verifies that an externally-tampered workspace directory (chmod 0o000
    // so the running process can't traverse into it) surfaces a clean Err
    // rather than a panic.
    //
    // Scenario: the user (or a hostile process running as the same uid)
    // strips the workspace's permissions between scan-complete and sweep.
    // Argos must surface "cannot access workspace" cleanly.

    use std::os::unix::fs::PermissionsExt;

    let harness = RegtestHarness::require();
    let temp_data_dir = tempfile::tempdir().expect("tempdir for workspace");
    let fixture = complete_scan_against_test_seed(&harness, &temp_data_dir, "argos-rw26").await;

    // Strip permissions on the leaf workspace directory. 0o000 blocks even
    // traversal — opening files inside fails because the directory has no
    // execute bit. Owned by the test process (we created it via Argos), so
    // we can chmod it back later.
    std::fs::set_permissions(
        &fixture.workspace_root,
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("chmod 0o000 on workspace root");

    // RAII guard: restore 0o700 on the workspace before the tempdir tries to
    // recursively delete it (otherwise the tempdir's cleanup would itself
    // fail with permission-denied). Declared after we apply 0o000 so it
    // drops first (LIFO) — before `temp_data_dir`'s Drop.
    struct RestorePerms<'a>(&'a std::path::Path);
    impl Drop for RestorePerms<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(
                self.0,
                std::fs::Permissions::from_mode(0o700),
            );
        }
    }
    let _restore = RestorePerms(&fixture.workspace_root);

    let request = SweepRequest {
        destination: fixture.destination_ua.clone(),
        memo: None,
        max_fee_zatoshis: None,
        donation_rate: None,
        donor_email: None,
    };

    let result = fixture.service.propose_sweep(&fixture.handle, request).await;
    let err = result.expect_err(
        "propose_sweep against a workspace with stripped permissions must return Err",
    );

    eprintln!("[regtest] propose_sweep failed as expected after chmod 0o000: {err}");
}
