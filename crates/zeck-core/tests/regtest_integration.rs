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
//
// Verifies that an HTTP/2-class mid-stream disconnect triggers the
// reconnect path in `run_wallet_sync_with_retry`, and that the resumed
// scan reaches the same final height as a baseline uninterrupted scan
// without duplicating discoveries.
//
// **What this test substitutes for a real GoAway:** the production retry
// loop in `scan.rs:run_wallet_sync_with_retry` decides whether to retry
// by substring-matching the error message against a fixed list of
// transport-class strings (`"transport error"`, `"h2 protocol error"`,
// `"GoAway"`, `"TimedOut"`, `"close_notify"`, `"UnexpectedEof"`). It does
// not inspect the underlying h2 frame. We therefore exercise the retry
// path by having `FakeLightwalletd` abort the stream with a
// `Status::unavailable` whose message contains both `"h2 protocol error"`
// and `"GoAway"` — this is exactly the contract the production code is
// written against. A real h2-frame-level GoAway is left for whenever the
// h2 crate exposes a server-side `send_goaway` API; the assertions here
// would not change.
//
// What the fixture does:
//   - Proxies all blocks 1..N from the real harness.
//   - At block N+1, returns the simulated GoAway error and sets its
//     "fault triggered" flag so the next reconnect sees clean behaviour.
//
// Assertions:
//   1. Scan reaches phase = Complete (i.e. the retry path succeeded).
//   2. Final `synced_to_height` equals the baseline uninterrupted scan
//      run against the bare harness — proves the resume cursor is sound.
//   3. Discovery list has no duplicates after the disconnect — proves
//      the per-scan dedup set in the pump loop survives reconnect.
#[cfg(feature = "argos-network")]
#[ignore = "requires the Argos network harness (tests/regtest/ booted, ARGOS_REGTEST_LIGHTWALLETD_URL exported)"]
#[tokio::test]
async fn goaway_mid_scan_reconnects_without_duplicate_emissions() {
    let harness = RegtestHarness::require();

    // Baseline: run a scan against the bare harness so we know what
    // `synced_to_height` and which discoveries the chain ought to produce.
    let baseline_dir = tempfile::tempdir().expect("temp data dir for baseline scan");
    let baseline = complete_scan_against_test_seed(&harness, &baseline_dir, "rn8-baseline")
        .await;
    let baseline_progress = baseline
        .service
        .get_scan_progress(&baseline.handle)
        .await
        .expect("baseline scan progress");
    let baseline_synced = baseline_progress.synced_to_height;
    let baseline_discoveries = baseline_progress.discoveries.len();
    drop(baseline);
    drop(baseline_dir);

    // Bring up the fixture in proxy mode with GoAway-after-3-blocks. The
    // regtest harness's chain is ~200 blocks after setup.sh, so 3 is well
    // inside the stream — the retry path has to fire.
    let fake = common::fake_lightwalletd::FakeLightwalletd::builder()
        .upstream(harness.lightwalletd_url().to_owned())
        .close_stream_after_blocks(3)
        .build()
        .await
        .expect("bind FakeLightwalletd on loopback");

    // Run the scan against the fixture URL instead of the harness URL.
    let fixture_dir = tempfile::tempdir().expect("temp data dir for fixture scan");
    let fixture_seed = harness.test_seed().to_owned();
    let runtime = argos_core::RuntimeScanConfig {
        seed_phrase: SecretString::new(fixture_seed.clone()),
        birthday: 1,
        num_accounts: Some(2),
        gap_limit: 5,
        lightwalletd_url: fake.url.clone(),
        data_dir: fixture_dir.path().to_path_buf(),
        network: ZeckNetwork::Testnet,
        label: "rn8-faulted".to_owned(),
    };
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
        .start_scan(scan_config, SecretString::new(fixture_seed))
        .await
        .expect("start_scan against FakeLightwalletd");

    // Poll to completion. Generous bound — the retry loop sleeps
    // SYNC_RETRY_DELAY_SECS (5s) between attempts.
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    let final_progress = loop {
        let progress = service
            .get_scan_progress(&handle)
            .await
            .expect("get_scan_progress");
        match progress.phase {
            ScanPhase::Complete => break progress,
            ScanPhase::Error => panic!(
                "[regtest] R-N8: scan errored instead of recovering via retry: {:?}",
                progress.error
            ),
            ScanPhase::Cancelled => panic!("[regtest] R-N8: scan unexpectedly cancelled"),
            _ => {
                if std::time::Instant::now() > deadline {
                    panic!(
                        "[regtest] R-N8: scan did not complete within 180s; last phase = {:?}",
                        progress.phase
                    );
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };

    assert_eq!(
        final_progress.synced_to_height, baseline_synced,
        "[regtest] R-N8: post-reconnect synced_to_height must equal the baseline; \
         got {:?}, baseline {:?}",
        final_progress.synced_to_height, baseline_synced
    );

    // Discovery dedup: every (account, kind, address) triple appears at most
    // once. The pump loop's seen-set lives in `ScanProgress.discoveries`
    // itself (it's an append-only log), so duplicates would surface here.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for d in &final_progress.discoveries {
        let key = format!("{:?}|{}|{}", d.pool, d.account_index, d.address);
        assert!(
            seen.insert(key.clone()),
            "[regtest] R-N8: duplicate discovery after reconnect: {key}"
        );
    }

    // Baseline produced N discoveries; the faulted run must produce the
    // same set. Equality on count is sufficient given the dedup assertion
    // above proves every entry is unique.
    assert_eq!(
        final_progress.discoveries.len(),
        baseline_discoveries,
        "[regtest] R-N8: discovery count differs from baseline (post-reconnect={}, baseline={})",
        final_progress.discoveries.len(),
        baseline_discoveries
    );

    eprintln!(
        "[regtest] R-N8 ok: scan recovered from simulated GoAway, reached {:?} (matches baseline)",
        final_progress.synced_to_height
    );
}

// ─── R-N9: Hostile compact block ────────────────────────────────────────────
//
// Verifies that a structurally-parseable but chain-invalid compact block
// surfaces as a clean error from `zcash_client_backend::sync` (not a
// panic), without corrupting the wallet DB for a subsequent scan.
//
// `FakeLightwalletd` mutates the `prev_hash` of the block at a configured
// height (XOR all bytes with 0xff). The block still parses — gRPC decode
// succeeds — but the chain-link check in librustzcash's sync rejects it
// because the block no longer links to its predecessor.
//
// Why XOR-prev_hash rather than e.g. a malformed commitment tree: the
// chain-link check fires earliest in the sync pipeline, gives a
// deterministic rejection point regardless of which blocks happened to
// contain notes for our test seed, and exercises the same error path a
// genuinely adversarial server would trigger by lying about the chain.
//
// Assertions:
//   1. The first scan ends in `ScanPhase::Error` — not Complete, not a
//      panic. The retry loop must NOT classify this as a transport-class
//      error (it isn't); the error must propagate.
//   2. A second scan against the bare harness (fresh workspace) reaches
//      the baseline `synced_to_height`. The faulted scan's database
//      lives in its own tempdir, so this also implicitly verifies that
//      writing to that DB and then dropping it leaves no global state
//      that pollutes the next workspace.
#[cfg(feature = "argos-network")]
#[ignore = "requires the Argos network harness (tests/regtest/ booted, ARGOS_REGTEST_LIGHTWALLETD_URL exported)"]
#[tokio::test]
async fn hostile_compact_block_rejected_cleanly() {
    let harness = RegtestHarness::require();

    // Baseline first (same pattern as R-N8) so we have a target.
    let baseline_dir = tempfile::tempdir().expect("temp data dir for baseline scan");
    let baseline = complete_scan_against_test_seed(&harness, &baseline_dir, "rn9-baseline")
        .await;
    let baseline_progress = baseline
        .service
        .get_scan_progress(&baseline.handle)
        .await
        .expect("baseline scan progress");
    let baseline_synced = baseline_progress.synced_to_height;
    drop(baseline);
    drop(baseline_dir);

    // Pick a height inside the funded range. Setup.sh funds the test seed
    // around block ~100; injecting at block 5 is safely past Sapling
    // activation (height 1) and well before any wallet-relevant block.
    let hostile_height: u64 = 5;
    let fake = common::fake_lightwalletd::FakeLightwalletd::builder()
        .upstream(harness.lightwalletd_url().to_owned())
        .inject_hostile_block_at_height(hostile_height)
        .build()
        .await
        .expect("bind FakeLightwalletd on loopback");

    let faulted_dir = tempfile::tempdir().expect("temp data dir for faulted scan");
    let seed = harness.test_seed().to_owned();
    let scan_config = ScanConfig {
        birthday: 1,
        num_accounts: Some(2),
        gap_limit: 5,
        lightwalletd_url: fake.url.clone(),
        data_dir: faulted_dir.path().to_path_buf(),
        network: ZeckNetwork::Testnet,
        label: "rn9-faulted".to_owned(),
    };
    let service = RecoveryService::new();
    let handle = service
        .start_scan(scan_config, SecretString::new(seed.clone()))
        .await
        .expect("start_scan against hostile FakeLightwalletd");

    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let faulted_progress = loop {
        let progress = service
            .get_scan_progress(&handle)
            .await
            .expect("get_scan_progress");
        match progress.phase {
            ScanPhase::Error => break progress,
            ScanPhase::Complete => panic!(
                "[regtest] R-N9: scan must NOT complete cleanly against a hostile chain"
            ),
            ScanPhase::Cancelled => panic!("[regtest] R-N9: scan unexpectedly cancelled"),
            _ => {
                if std::time::Instant::now() > deadline {
                    panic!(
                        "[regtest] R-N9: scan neither errored nor completed within 120s; \
                         last phase = {:?}",
                        progress.phase
                    );
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };

    let err_text = faulted_progress
        .error
        .as_ref()
        .expect("[regtest] R-N9: scan ended in Error but error field was empty")
        .to_string();
    assert!(
        !err_text.is_empty(),
        "[regtest] R-N9: error message must be non-empty"
    );
    // The exact wording comes from librustzcash; we just check we got a
    // *useful* message, not a generic panic backtrace.
    assert!(
        !err_text.contains("panicked"),
        "[regtest] R-N9: scan must surface an Err, not propagate a panic: {err_text}"
    );
    eprintln!("[regtest] R-N9 ok: hostile block rejected with: {err_text}");

    drop(service);
    drop(faulted_dir);
    drop(fake);

    // Subsequent clean rescan against the bare harness reaches baseline.
    // Uses a fresh workspace so we're testing "no global state pollution",
    // not resume.
    let recovery_dir = tempfile::tempdir().expect("temp data dir for recovery scan");
    let recovery = complete_scan_against_test_seed(&harness, &recovery_dir, "rn9-recovery")
        .await;
    let recovery_progress = recovery
        .service
        .get_scan_progress(&recovery.handle)
        .await
        .expect("recovery scan progress");
    assert_eq!(
        recovery_progress.synced_to_height, baseline_synced,
        "[regtest] R-N9: post-incident clean scan must reach baseline height \
         (got {:?}, baseline {:?})",
        recovery_progress.synced_to_height, baseline_synced
    );
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

// ─── R-S26: Reorg during scan ───────────────────────────────────────────────
#[ignore = "requires the Argos network harness (tests/regtest/ booted, ARGOS_REGTEST_LIGHTWALLETD_URL exported)"]
#[tokio::test]
async fn reorg_during_scan_invalidates_and_rescans_affected_range() {
    // Verifies that a chain reorg between two scans is detected and handled
    // by `zcash_client_backend`'s sync layer: the wallet DB rolls back to
    // the common ancestor and re-scans the new fork, reaching at least the
    // new chain tip on completion.
    //
    // ## Sequence
    //
    //   1. Initial scan via the shared helper — wallet observes the
    //      harness's current tip.
    //   2. Drive a regtest reorg via zcash-cli:
    //        - `invalidateblock <hash@tip-5>` rolls the active chain back
    //          to height (tip - 6).
    //        - `generate 10` mines a strictly-longer fork, ending at
    //          (tip - 6 + 10) = tip + 4.
    //   3. Brief sleep so lightwalletd's polling loop observes the new
    //      view before Argos's next sync. The exact delay depends on
    //      lightwalletd's poll interval (~5s in the harness config); 3s
    //      is the conservative default — bump it via the test wrapper if
    //      the hands-on validation shows flakiness.
    //   4. Second scan against the same workspace. The conflict-
    //      cancellation logic (R-W24) cleans up the first session; the
    //      new session resumes from the workspace's `fully_scanned_height`
    //      and forces sync's reorg-detection path.
    //   5. Final progress must report a `synced_to_height` at least as
    //      high as the new chain tip, and the balance must be unchanged
    //      (funding tx is at ~height 201 per setup.sh, well below the
    //      5-block reorg window).
    //
    // ## What this test does NOT cover
    //
    // The strong version of "final balance matches post-reorg ground
    // truth" would put a transaction *inside* the reorged range and
    // verify Argos sees the post-reorg version, not the pre-reorg one.
    // That needs a wallet that can send funds on regtest (and therefore
    // a working spend pipeline against the harness), which is a separate
    // future PR. The current test pins the structural property: scan
    // doesn't crash on reorg + tip advances + balance below the reorg
    // window is preserved.

    let harness = RegtestHarness::require();
    let temp_data_dir = tempfile::tempdir().expect("tempdir");
    let fixture =
        complete_scan_against_test_seed(&harness, &temp_data_dir, "argos-rs26-initial").await;

    let pre = fixture
        .service
        .get_scan_progress(&fixture.handle)
        .await
        .expect("get_scan_progress after initial scan");
    let pre_tip = pre
        .synced_to_height
        .expect("[regtest] initial scan must populate synced_to_height");
    let pre_balance: u64 = pre.accounts.iter().map(|a| a.total_zatoshis).sum();
    eprintln!("[regtest] pre-reorg: tip={pre_tip}, balance={pre_balance}");

    let invalidate_height = pre_tip.saturating_sub(5);
    let invalidate_hash = zcashd_cli(&["getblockhash", &invalidate_height.to_string()]);
    eprintln!(
        "[regtest] invalidating block @ height {invalidate_height} (hash {invalidate_hash})",
    );
    let _ = zcashd_cli(&["invalidateblock", &invalidate_hash]);
    let _ = zcashd_cli(&["generate", "10"]);

    // Let lightwalletd's polling loop observe the new tip.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let new_chain_tip: u64 = zcashd_cli(&["getblockcount"])
        .parse()
        .expect("[regtest] parse getblockcount output");
    assert!(
        new_chain_tip > pre_tip,
        "[regtest] post-reorg chain tip must exceed pre-reorg tip; got new={new_chain_tip}, pre={pre_tip}"
    );
    eprintln!("[regtest] post-reorg new chain tip: {new_chain_tip}");

    // Second scan against the same workspace forces sync's reorg path.
    let scan_config = ScanConfig {
        birthday: 1,
        num_accounts: Some(2),
        gap_limit: 5,
        lightwalletd_url: harness.lightwalletd_url().to_owned(),
        data_dir: temp_data_dir.path().to_path_buf(),
        network: ZeckNetwork::Testnet,
        label: "argos-rs26-post".to_owned(),
    };
    let handle = fixture
        .service
        .start_scan(
            scan_config,
            SecretString::new(harness.test_seed().to_owned()),
        )
        .await
        .expect("post-reorg start_scan must succeed");

    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let final_progress = loop {
        let progress = fixture
            .service
            .get_scan_progress(&handle)
            .await
            .expect("get_scan_progress during post-reorg scan");
        match progress.phase {
            ScanPhase::Complete => break progress,
            ScanPhase::Error => panic!(
                "[regtest] post-reorg scan errored: {:?} — \
                 zcash_client_backend's chain reconciliation regressed?",
                progress.error
            ),
            ScanPhase::Cancelled => {
                panic!("[regtest] post-reorg scan unexpectedly cancelled")
            }
            _ => {
                if std::time::Instant::now() > deadline {
                    panic!(
                        "[regtest] post-reorg scan did not complete within 120s; \
                         last phase = {:?}",
                        progress.phase
                    );
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };

    let post_tip = final_progress
        .synced_to_height
        .expect("[regtest] post-reorg scan must populate synced_to_height");
    let post_balance: u64 = final_progress.accounts.iter().map(|a| a.total_zatoshis).sum();
    eprintln!("[regtest] post-reorg: tip={post_tip}, balance={post_balance}");

    assert!(
        post_tip >= new_chain_tip,
        "[regtest] post-reorg scan must reach the new chain tip; \
         post_tip={post_tip}, new_chain_tip={new_chain_tip} — sync's reorg \
         reconciliation didn't roll forward to the new fork"
    );
    assert_eq!(
        post_balance, pre_balance,
        "[regtest] funding tx is below the reorg window; balance must be unchanged"
    );

    eprintln!("[regtest] reorg detected and rescanned successfully");
}

/// Execute a zcash-cli command against the harness's zcashd-regtest container
/// and return stdout (trimmed). Used by R-S26 to drive chain manipulation
/// (invalidateblock, generate, getblockhash) via RPC.
///
/// Defaults to `docker exec` against the well-known container name from
/// `tests/regtest/docker-compose.yml`. Bare-metal contributors (those
/// running zcashd locally rather than via the docker harness) override the
/// wrapper command line via:
///
///     ARGOS_REGTEST_ZCASH_CLI="zcash-cli -conf=/path/to/zcash.conf"
///
/// The override is parsed by whitespace-splitting and so cannot contain
/// args with embedded spaces — sufficient for typical zcash-cli usage.
///
/// Panics rather than returning Result because every failure here is a
/// hard test-setup error (docker missing, container down, zcash-cli
/// returning non-zero) — the C2 tests are already `#[ignore]`'d so the
/// panic only surfaces under `cargo test --ignored`.
fn zcashd_cli(args: &[&str]) -> String {
    let wrapper = std::env::var("ARGOS_REGTEST_ZCASH_CLI").unwrap_or_else(|_| {
        "docker exec argos-zcashd-regtest zcash-cli -conf=/srv/zcashd/.zcash/zcash.conf".to_owned()
    });
    let parts: Vec<&str> = wrapper.split_whitespace().collect();
    let (program, base_args) = parts
        .split_first()
        .expect("[regtest] ARGOS_REGTEST_ZCASH_CLI must not be empty");
    let output = std::process::Command::new(program)
        .args(base_args)
        .args(args)
        .output()
        .unwrap_or_else(|err| {
            panic!("[regtest] failed to spawn `{program}` for zcash-cli: {err}")
        });
    if !output.status.success() {
        panic!(
            "[regtest] zcash-cli {args:?} failed: status={:?}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
    }
    String::from_utf8(output.stdout)
        .expect("[regtest] zcash-cli stdout was not valid UTF-8")
        .trim()
        .to_owned()
}

// ─── R-S27: Crash mid-scan resume ───────────────────────────────────────────
//
// Deferred. Two structural points make a runtime version of this test
// substantially weaker than it looks on the surface:
//
//   1. **In-process cancellation is not a crash.** The cheap version of
//      this test — spawn `RecoveryService::start_scan`, drop the handle
//      partway through, recreate the service on the same workspace,
//      observe resume — only exercises the Drop path of the sync task,
//      which is well-behaved by construction (every commit to the wallet
//      DB is its own SQLite transaction). It does NOT exercise the
//      genuine-crash failure mode where the process disappears between a
//      sync batch's "scanned blocks" emission and its DB commit. That
//      window is exactly where data corruption would happen if it could,
//      and only an actual SIGKILL of a subprocess can land inside it.
//
//   2. **Subprocess SIGKILL needs argos-cli scaffolding we don't have.**
//      The honest version spawns `argos-cli scan` with `--data-dir
//      <tmpdir>` against the harness URL, parses its stdout to know how
//      far it got, sends SIGKILL after N blocks, then re-runs it and
//      checks `fully_scanned_height` came up where the first run left
//      off. That needs (a) a stable `argos-cli scan` JSON-progress mode
//      (today the output is human-readable lines), (b) the cli binary on
//      `$PATH` during `cargo test` runs, and (c) a sweep of the
//      ARGOS_DATA_DIR env-var plumbing through tests. All three are real
//      changes outside this PR's scope.
//
// The resume invariant itself is exercised on the unit-test side: R-W1
// through R-W20 in `workspace.rs` cover the `(seed, birthday, network)`
// keying that determines what counts as "the same workspace" for resume,
// and `fully_scanned_height` comes straight from `zcash_client_sqlite`
// (whose own test suite covers the per-batch commit boundary). The C2
// gap is specifically "real SIGKILL against real subprocess" — see also
// the C3 testnet smoke checklist item "Restart the GUI mid-scan" in
// `docs/superpowers/test-plans/recovery-resilience.md`, which is the
// manual cover for this row until the subprocess scaffolding lands.
#[ignore = "deferred: needs argos-cli subprocess + JSON-progress mode for honest SIGKILL — see comment"]
#[test]
fn crash_mid_scan_resumes_from_fully_scanned_height() {
    let _harness = RegtestHarness::require();
    panic!(
        "[regtest] R-S27 deferred: see comment above. Convert after argos-cli \
         gains a JSON progress mode and the test harness can spawn it as a \
         subprocess with deterministic SIGKILL timing."
    );
}

// ─── R-S29: Crash mid-broadcast ─────────────────────────────────────────────
//
// Deferred. The no-double-spend property at the broadcast layer requires
// the SIGKILL to land BETWEEN two per-account sweep broadcasts. Our
// current harness funds exactly one account (`tests/regtest/setup.sh`
// sends to the test seed's account-0 transparent address), so there's
// only one broadcast in a sweep — no "mid-broadcast" window to interrupt.
//
// Converting this stub end-to-end needs one of:
//
//   - Extending setup.sh to fund N accounts so a single sweep produces
//     N per-account broadcasts, with the SIGKILL targeting the gap
//     between #1 and #2.
//   - A test-only hook in `RecoveryService::execute_sweep_for_session`
//     that can pause / panic at a chosen point between broadcasts,
//     which the test then drives deterministically.
//
// Both are real work and warrant their own PR. Filing this stub as
// `panic!("deferred ...")` rather than `unimplemented!()` so the test
// produces a clear, actionable message under `cargo test --ignored`
// instead of looking like an unfinished placeholder.
#[ignore = "deferred: needs multi-account funding or a sweep-broadcast pause hook — see comment"]
#[test]
fn crash_mid_broadcast_does_not_double_spend_on_resume() {
    let _harness = RegtestHarness::require();
    panic!(
        "[regtest] R-S29 deferred: see comment above. Convert after either \
         extending tests/regtest/setup.sh to fund multiple accounts, or \
         adding a test-only pause hook in execute_sweep_for_session."
    );
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

// ─── FakeLightwalletd fixture smoke test ────────────────────────────────────
//
// Validates the proto-codegen and tonic-server plumbing for the in-process
// `FakeLightwalletd` fixture (`tests/common/fake_lightwalletd.rs`). Boots the
// fixture in pure-skeleton mode (no upstream) and confirms a real Argos
// gRPC client can probe it, get back the configured chain identity, and pass
// `validate_lightwalletd_network` as a regtest server under the
// `argos-network` feature.
//
// `#[ignore]` so it doesn't run in default `cargo test`; the fault-injection
// follow-up PR will lift the gate once R-N8/R-N9 land.
#[cfg(feature = "argos-network")]
#[ignore = "fixture smoke test; run with --ignored --features argos-network"]
#[tokio::test]
async fn fake_lightwalletd_smoke() {
    use argos_core::lightwalletd::{
        probe_lightwalletd_endpoints, validate_lightwalletd_network,
    };
    use argos_core::models::ZeckNetwork;

    let fake = common::fake_lightwalletd::FakeLightwalletd::builder()
        .chain_name("regtest")
        .sapling_activation_height(1)
        .block_height(42)
        .build()
        .await
        .expect("bind FakeLightwalletd on loopback");

    let (_client, endpoint, info) = probe_lightwalletd_endpoints(&fake.url)
        .await
        .expect("Argos client probes FakeLightwalletd cleanly");

    assert_eq!(endpoint, fake.url);
    assert_eq!(info.chain_name, "regtest");
    assert_eq!(info.sapling_activation_height, 1);
    assert_eq!(info.block_height, 42);

    validate_lightwalletd_network(ZeckNetwork::Testnet, &info)
        .expect("regtest chain must validate as Testnet under argos-network");
}
