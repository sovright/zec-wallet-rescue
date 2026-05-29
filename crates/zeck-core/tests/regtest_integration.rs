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
// Deferred. Triggering a GoAway frame specifically (not a TCP RST) requires
// either:
//
//   - A custom lightwalletd build patched to emit `GOAWAY` on a timer (the
//     stock electriccoinco/lightwalletd image does not expose this knob), or
//   - An HTTP/2-aware MITM proxy in front of lightwalletd that injects a
//     well-formed GOAWAY frame at a chosen point in the stream. Building
//     one with `h2` directly is ~200 lines and warrants its own crate.
//
// A pure TCP-drop proxy would *also* exercise `run_wallet_sync_with_retry`
// (the retry loop catches transport-class errors regardless of which
// HTTP/2 layer raised them), but the proximate cause would be
// `UnexpectedEof`, not `NO_ERROR` GOAWAY. The CLAUDE.md note that
// motivated this row specifically calls out GOAWAY as the production
// failure mode against zec.rocks, so substituting TCP-drop would not
// honestly verify the documented behaviour — it would just verify that
// *some* mid-stream disconnect is retried.
//
// Best done in a follow-up PR with a shared `FakeLightwalletd` gRPC
// fixture (also unblocks R-N9) plus an h2-frame injection layer.
#[ignore = "deferred: needs an h2 GOAWAY-injecting proxy or patched lightwalletd — see comment"]
#[test]
fn goaway_mid_scan_reconnects_without_duplicate_emissions() {
    let _harness = RegtestHarness::require();
    panic!(
        "[regtest] R-N8 deferred: see comment above. Converting needs either \
         a custom lightwalletd build or an h2-aware MITM proxy that injects \
         a GOAWAY frame mid-stream."
    );
}

// ─── R-N9: Hostile compact block ────────────────────────────────────────────
//
// Deferred. Verifying that a malformed compact block surfaces a clean Err
// from `zcash_client_backend::sync` requires a fake `CompactTxStreamer`
// gRPC server that emits a crafted block whose internal structure is
// parseable but semantically invalid (e.g. wrong commitment-tree root,
// wrong tx hash). Two substantial pieces of infrastructure:
//
//   - A tonic service stub implementing the `CompactTxStreamer` interface
//     (GetLightdInfo + GetBlockRange + GetCompactBlock at minimum).
//   - A crafted compact-block payload — requires understanding the
//     `zcash_protocol::consensus` block format well enough to build a
//     block that fails witness validation rather than rejecting at parse.
//
// Best done in a follow-up PR with a shared `FakeLightwalletd` test
// fixture; the same scaffolding then unblocks R-N8's GoAway-specific
// frame injection (currently exercised here at the TCP-drop layer
// instead — see R-N8 above for the simplification rationale).
#[ignore = "deferred: needs a FakeLightwalletd gRPC service + crafted block payload — see comment"]
#[test]
fn hostile_compact_block_rejected_cleanly() {
    let _harness = RegtestHarness::require();
    panic!(
        "[regtest] R-N9 deferred: see comment above. Convert after the \
         FakeLightwalletd fixture lands (shared with R-N8 follow-up)."
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
// Implemented in #92 (helper-binary scaffolding) + this PR.
//
// Spawns `argos-scan-helper` as a subprocess against the booted regtest
// harness, watches its stdout JSON-line stream for a `block` event past a
// configurable threshold, delivers SIGKILL via `tokio::process::Child::start_kill`,
// then re-spawns the same helper with the same `--data-dir`. The second run
// must reach `Complete` with `total_zatoshis` matching a baseline
// uninterrupted scan.
//
// Why this exercises the production property: the SIGKILL lands inside the
// subprocess's running event loop, not via in-process task cancellation.
// Whether it lands between a batch's "scanned blocks" emission and the
// corresponding DB commit is timing-dependent, but the test runs the kill
// many blocks into the scan — well past the first batch boundary — so over
// runs the kill window will land in different places. The assertion that
// matters is structural: the second run reaches the same final state as
// the baseline. Any failure to resume (corruption, lost cursor, double-
// counting) would surface as `total_zatoshis` divergence.
#[cfg(feature = "argos-network")]
#[ignore = "requires the Argos network harness (tests/regtest/ booted, ARGOS_REGTEST_LIGHTWALLETD_URL exported)"]
#[tokio::test]
async fn crash_mid_scan_resumes_from_fully_scanned_height() {
    use common::subprocess_driver::{HelperEvent, HelperSpawn};

    let harness = RegtestHarness::require();

    // ─── Baseline: uninterrupted scan, capture total_zatoshis ──────────────
    let baseline_dir = tempfile::tempdir().expect("baseline tempdir");
    let baseline_handle = HelperSpawn::new(
        env!("CARGO_BIN_EXE_argos-scan-helper"),
        harness.test_seed().to_owned(),
    )
    .arg_value("--data-dir", baseline_dir.path().display().to_string())
    .arg_value("--lightwalletd-url", harness.lightwalletd_url().to_owned())
    .arg_value("--birthday", "1")
    .arg_value("--num-accounts", "2")
    .arg_value("--gap-limit", "5")
    .arg_value("--label", "rs27-baseline")
    .spawn()
    .await
    .expect("spawn scan-helper for baseline");

    let (_baseline_status, baseline_events) = baseline_handle
        .wait_for_exit()
        .await
        .expect("baseline scan-helper must run to completion");

    let baseline_total = baseline_events
        .iter()
        .rev()
        .find_map(|e| match e {
            HelperEvent::Complete { total_zatoshis } => Some(*total_zatoshis),
            _ => None,
        })
        .expect("baseline scan must emit a Complete event with total_zatoshis");
    assert!(
        baseline_total > 0,
        "[regtest] R-S27: baseline scan found zero funds; setup.sh did not run?"
    );

    // ─── First crash run: kill the subprocess after >= SCAN_KILL_AT blocks ─
    //
    // The threshold is well past the first batch boundary in librustzcash's
    // default batch size (~100 blocks), so the kill lands inside one of the
    // mid-scan windows R-S27 is meant to exercise. Regtest setup.sh tops the
    // chain at ~200 blocks; 50 leaves comfortable headroom on either side.
    const SCAN_KILL_AT: u64 = 50;

    let resume_dir = tempfile::tempdir().expect("resume tempdir");
    let mut crash_handle = HelperSpawn::new(
        env!("CARGO_BIN_EXE_argos-scan-helper"),
        harness.test_seed().to_owned(),
    )
    .arg_value("--data-dir", resume_dir.path().display().to_string())
    .arg_value("--lightwalletd-url", harness.lightwalletd_url().to_owned())
    .arg_value("--birthday", "1")
    .arg_value("--num-accounts", "2")
    .arg_value("--gap-limit", "5")
    .arg_value("--label", "rs27-crash")
    .spawn()
    .await
    .expect("spawn scan-helper for crash run");

    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let kill_block = crash_handle
        .wait_for(deadline, |events| {
            events.iter().rev().find_map(|e| match e {
                HelperEvent::Block { scanned_to } if *scanned_to >= SCAN_KILL_AT => {
                    Some(*scanned_to)
                }
                _ => None,
            })
        })
        .await
        .expect("scan-helper must emit a block event past the kill threshold");

    let kill_status = crash_handle
        .sigkill_and_wait()
        .await
        .expect("SIGKILL must reap the subprocess");
    assert!(
        !kill_status.success(),
        "[regtest] R-S27: subprocess should have died from SIGKILL, but exited cleanly: {kill_status:?}"
    );
    eprintln!("[regtest] R-S27: SIGKILLed first run at block {kill_block}");

    // ─── Resume run: same --data-dir, expect Complete reaching baseline ────
    let resume_handle = HelperSpawn::new(
        env!("CARGO_BIN_EXE_argos-scan-helper"),
        harness.test_seed().to_owned(),
    )
    .arg_value("--data-dir", resume_dir.path().display().to_string())
    .arg_value("--lightwalletd-url", harness.lightwalletd_url().to_owned())
    .arg_value("--birthday", "1")
    .arg_value("--num-accounts", "2")
    .arg_value("--gap-limit", "5")
    .arg_value("--label", "rs27-crash") // same label keeps the workspace key identical
    .spawn()
    .await
    .expect("spawn scan-helper for resume");

    let (resume_status, resume_events) = resume_handle
        .wait_for_exit()
        .await
        .expect("resume scan-helper must run to completion");
    assert!(
        resume_status.success(),
        "[regtest] R-S27: resume run did not exit cleanly: {resume_status:?}"
    );

    let resume_total = resume_events
        .iter()
        .rev()
        .find_map(|e| match e {
            HelperEvent::Complete { total_zatoshis } => Some(*total_zatoshis),
            _ => None,
        })
        .expect("resume scan must emit a Complete event");
    assert_eq!(
        resume_total, baseline_total,
        "[regtest] R-S27: resume total_zatoshis ({resume_total}) must match baseline ({baseline_total})"
    );

    eprintln!(
        "[regtest] R-S27 ok: SIGKILL at block {kill_block}, resume reached {} zatoshis (matches baseline)",
        resume_total
    );
}

// ─── R-S29: Crash mid-broadcast ─────────────────────────────────────────────
//
// Implemented in #92 (sweep-helper + pause hook) + this PR (multi-account
// funding in setup.sh).
//
// `argos-sweep-helper` is spawned with `--pause-millis-between-broadcasts
// 30000` so the per-account broadcast loop sleeps 30s between accounts. The
// test parent waits for the `sweep_starting` event, then sleeps long enough
// (~8s) for the first broadcast to land in the wallet DB but not nearly
// enough to exit the library's 30s pause. SIGKILL.
//
// On the resume run, the helper does a fresh scan against the same
// `--data-dir`. The account that was already swept on the killed run has
// near-zero balance after sync (because its sweep tx persisted in the
// wallet DB and was included in the chain by the harness's miner), so the
// second sweep produces exactly **one** broadcast: for the account that
// was *not* swept on the killed run.
//
// Assertion: second-run `broadcast_count` is exactly 1 (the
// not-yet-swept account). Two broadcasts would prove the first run's
// effect was lost; zero broadcasts would prove the second account had
// already been swept (impossible if the kill landed in the gap).
//
// Skip-condition: if setup.sh has not been re-run with the new multi-account
// default (i.e. `ARGOS_REGTEST_TEST_T_ADDR_1` is not exported), the test
// emits a skip notice rather than failing — there's no point asserting a
// multi-broadcast property against a single-broadcast fixture.
#[cfg(feature = "argos-network")]
#[ignore = "requires the Argos network harness (tests/regtest/ booted, ARGOS_REGTEST_LIGHTWALLETD_URL exported, setup.sh re-run with multi-account funding)"]
#[tokio::test]
async fn crash_mid_broadcast_does_not_double_spend_on_resume() {
    use common::subprocess_driver::{HelperEvent, HelperSpawn};

    let harness = RegtestHarness::require();

    // Multi-account funding gate: PR B's setup.sh exports
    // ARGOS_REGTEST_TEST_T_ADDR_1 when account 1 was funded. If a
    // contributor is running against an older setup.sh, fail with a
    // clear message rather than silently producing a meaningless result.
    if std::env::var("ARGOS_REGTEST_TEST_T_ADDR_1").is_err() {
        panic!(
            "[regtest] R-S29 requires multi-account funding. Re-run \
             tests/regtest/setup.sh (which now funds accounts 0 and 1 by \
             default) and export ARGOS_REGTEST_TEST_T_ADDR_1."
        );
    }

    // Derive account-2's UA from the test seed as the sweep destination.
    // (Both account 0 and account 1 are sources; account 2 is the
    // destination, which keeps the sweep deterministic regardless of which
    // source account is processed first.)
    let seed = SecretString::new(harness.test_seed().to_owned());
    let accounts = derive_accounts(&seed, ZeckNetwork::Testnet, 3)
        .expect("derive 3 accounts to choose a sweep destination outside the funded set");
    let destination_ua = accounts[2].unified_address.clone();

    let crash_dir = tempfile::tempdir().expect("crash tempdir");

    // ─── First run: spawn, wait for sweep_starting, sleep, SIGKILL ────────
    let mut crash_handle = HelperSpawn::new(
        env!("CARGO_BIN_EXE_argos-sweep-helper"),
        harness.test_seed().to_owned(),
    )
    .arg_value("--data-dir", crash_dir.path().display().to_string())
    .arg_value("--lightwalletd-url", harness.lightwalletd_url().to_owned())
    .arg_value("--destination-ua", destination_ua.clone())
    .arg_value("--birthday", "1")
    .arg_value("--num-accounts", "3") // 0, 1 funded; 2 is the destination
    .arg_value("--gap-limit", "5")
    .arg_value("--label", "rs29-crash")
    .arg_value("--pause-millis-between-broadcasts", "30000")
    .spawn()
    .await
    .expect("spawn sweep-helper for crash run");

    let starting_deadline = std::time::Instant::now() + Duration::from_secs(180);
    crash_handle
        .wait_for(starting_deadline, |events| {
            events.iter().find_map(|e| match e {
                HelperEvent::SweepStarting => Some(()),
                _ => None,
            })
        })
        .await
        .expect("sweep-helper must emit SweepStarting within 180s");

    // Give the first broadcast time to land in the wallet DB. 8s comfortably
    // exceeds typical regtest broadcast latency (<1s) and is well inside
    // the helper's 30s pause window.
    tokio::time::sleep(Duration::from_secs(8)).await;

    let kill_status = crash_handle
        .sigkill_and_wait()
        .await
        .expect("SIGKILL must reap sweep-helper");
    assert!(
        !kill_status.success(),
        "[regtest] R-S29: subprocess should have died from SIGKILL, exited cleanly instead: {kill_status:?}"
    );
    eprintln!("[regtest] R-S29: SIGKILLed first run during pause between broadcasts");

    // ─── Resume run: same --data-dir, expect exactly 1 broadcast ──────────
    let resume_handle = HelperSpawn::new(
        env!("CARGO_BIN_EXE_argos-sweep-helper"),
        harness.test_seed().to_owned(),
    )
    .arg_value("--data-dir", crash_dir.path().display().to_string())
    .arg_value("--lightwalletd-url", harness.lightwalletd_url().to_owned())
    .arg_value("--destination-ua", destination_ua)
    .arg_value("--birthday", "1")
    .arg_value("--num-accounts", "3")
    .arg_value("--gap-limit", "5")
    .arg_value("--label", "rs29-crash") // identical workspace key
    .arg_value("--pause-millis-between-broadcasts", "0")
    .spawn()
    .await
    .expect("spawn sweep-helper for resume");

    let (resume_status, resume_events) = resume_handle
        .wait_for_exit()
        .await
        .expect("resume sweep-helper must run to completion");
    assert!(
        resume_status.success(),
        "[regtest] R-S29: resume run did not exit cleanly: {resume_status:?}"
    );

    let resume_broadcasts: Vec<u32> = resume_events
        .iter()
        .filter_map(|e| match e {
            HelperEvent::Broadcast { source_account, .. } => Some(*source_account),
            _ => None,
        })
        .collect();
    assert_eq!(
        resume_broadcasts.len(),
        1,
        "[regtest] R-S29: resume run must produce exactly 1 broadcast (the \
         not-yet-swept account); got {} broadcasts from accounts {:?}",
        resume_broadcasts.len(),
        resume_broadcasts
    );

    eprintln!(
        "[regtest] R-S29 ok: resume swept exactly the not-yet-swept account ({})",
        resume_broadcasts[0]
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

// ─── Helper-binary smoke tests (scaffolding for R-S27 / R-S29) ──────────────
//
// These tests prove that the `argos-scan-helper` and `argos-sweep-helper`
// binaries can be spawned, parse their CLI, talk to the real harness, and
// emit the documented stdout schema end-to-end. They run against the bare
// harness (no fault injection); the actual R-S27 and R-S29 tests in the
// next PR will exercise SIGKILL behaviour on top of this scaffolding.
//
// Both gated `#[ignore]` like every other C2 test — only run with
// `--features argos-network -- --ignored` after `tests/regtest/setup.sh`
// has funded the test seed.

#[cfg(feature = "argos-network")]
#[ignore = "scan-helper smoke test; requires the booted regtest harness"]
#[tokio::test]
async fn argos_scan_helper_smoke() {
    use common::subprocess_driver::{HelperEvent, HelperSpawn};

    let harness = RegtestHarness::require();
    let temp = tempfile::tempdir().expect("temp data dir for scan-helper smoke");

    let mut handle = HelperSpawn::new(
        env!("CARGO_BIN_EXE_argos-scan-helper"),
        harness.test_seed().to_owned(),
    )
    .arg_value("--data-dir", temp.path().display().to_string())
    .arg_value("--lightwalletd-url", harness.lightwalletd_url().to_owned())
    .arg_value("--birthday", "1")
    .arg_value("--num-accounts", "2")
    .arg_value("--gap-limit", "5")
    .arg_value("--label", "smoke")
    .spawn()
    .await
    .expect("spawn argos-scan-helper");

    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let total = handle
        .wait_for(deadline, |events| {
            events.iter().find_map(|e| match e {
                HelperEvent::Complete { total_zatoshis } => Some(*total_zatoshis),
                _ => None,
            })
        })
        .await
        .expect("scan-helper must reach Complete within 120s");

    assert!(
        total > 0,
        "[regtest] scan-helper smoke: setup.sh should have funded the test seed; got 0 zatoshis"
    );

    // Confirm the helper observed a transition through ScanningShielded —
    // proves the stdout schema covers phase transitions, not just final
    // events.
    let saw_shielded = handle.events().iter().any(|e| matches!(
        e,
        HelperEvent::Phase { phase } if phase == "scanning_shielded"
    ));
    assert!(
        saw_shielded,
        "[regtest] scan-helper smoke: expected a `scanning_shielded` phase event"
    );

    let (status, _events) = handle
        .wait_for_exit()
        .await
        .expect("scan-helper must exit cleanly after Complete");
    assert!(
        status.success(),
        "[regtest] scan-helper exit status was not success: {status:?}"
    );
}

#[cfg(feature = "argos-network")]
#[ignore = "sweep-helper smoke test; requires the booted regtest harness"]
#[tokio::test]
async fn argos_sweep_helper_smoke() {
    use common::subprocess_driver::{HelperEvent, HelperSpawn};

    let harness = RegtestHarness::require();
    let temp = tempfile::tempdir().expect("temp data dir for sweep-helper smoke");

    // Derive account-1's UA from the test seed — same trick the workspace
    // tests use to get a syntactically-valid UA without needing a second
    // funded seed in the harness.
    let seed = SecretString::new(harness.test_seed().to_owned());
    let accounts = derive_accounts(&seed, ZeckNetwork::Testnet, 2)
        .expect("derive_accounts for sweep destination");
    let destination_ua = accounts[1].unified_address.clone();

    let mut handle = HelperSpawn::new(
        env!("CARGO_BIN_EXE_argos-sweep-helper"),
        harness.test_seed().to_owned(),
    )
    .arg_value("--data-dir", temp.path().display().to_string())
    .arg_value("--lightwalletd-url", harness.lightwalletd_url().to_owned())
    .arg_value("--destination-ua", destination_ua)
    .arg_value("--birthday", "1")
    .arg_value("--num-accounts", "2")
    .arg_value("--gap-limit", "5")
    .arg_value("--label", "smoke-sweep")
    // No pause for the smoke test; just prove the end-to-end flow.
    .arg_value("--pause-millis-between-broadcasts", "0")
    .spawn()
    .await
    .expect("spawn argos-sweep-helper");

    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    let broadcast_count = handle
        .wait_for(deadline, |events| {
            events.iter().find_map(|e| match e {
                HelperEvent::SweepComplete { broadcast_count } => Some(*broadcast_count),
                _ => None,
            })
        })
        .await
        .expect("sweep-helper must reach SweepComplete within 240s");

    // setup.sh as-of this PR funds only account 0, so we expect exactly one
    // broadcast. The R-S29 PR will extend setup.sh to fund 2 accounts.
    assert!(
        broadcast_count >= 1,
        "[regtest] sweep-helper smoke: expected at least one broadcast"
    );

    // Confirm we observed the SweepStarting marker — R-S29 will use that as
    // its kill signal.
    let saw_starting = handle
        .events()
        .iter()
        .any(|e| matches!(e, HelperEvent::SweepStarting));
    assert!(
        saw_starting,
        "[regtest] sweep-helper smoke: expected SweepStarting event"
    );

    let (status, _events) = handle
        .wait_for_exit()
        .await
        .expect("sweep-helper must exit cleanly after SweepComplete");
    assert!(
        status.success(),
        "[regtest] sweep-helper exit status was not success: {status:?}"
    );
}
