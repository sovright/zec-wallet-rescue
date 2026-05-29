# Recovery resilience test plan

Companion to the sweep + donation tests already on PR #66. This plan covers
everything *outside* sweep + donation that determines whether a real user
recovers their funds: the scan side, the network resilience layer, resume /
crash robustness, workspace integrity, and the long-running scan behaviour.

The structure follows the [Argos threat model](../../THREAT_MODEL.md) — each
test maps to a threat row (`T-S*`, `T-N*`, `T-L*`, `T-B*`) or to a stated
fund-recovery invariant.

> **Scoping note:** sweep + donation edge cases (donation threshold,
> two-pass fee convergence, donation memo, donation `Err`-fallback, GUI form
> reset) live on PR #66's own test surface. This plan is the parallel branch
> for everything that isn't sweep + donation, so reviews stay focused.

## Categories

| Category | Where it runs | How to invoke |
|---|---|---|
| **C1** Unit tests, no network | CI on every push/PR | `cargo test --workspace` |
| **C2** Integration tests against a local regtest node | Local only, opt-in | `cargo test --workspace --features argos-network -- --ignored` after booting `tests/regtest/`. The `argos-network` feature relaxes `validate_lightwalletd_network` for the local chain; without it Argos refuses to talk to a regtest server. |
| **C3** Testnet smoke flow | Local, manual; required before a release tag | Documented checklist in this file |
| **C4** Mainnet small-amount gate | Manual, one-off pre-release | Section below |

CI today is C1 only. C2 is opt-in by design — the regtest node is heavy to
boot and CI runners don't keep it warm. C3/C4 are humans driving the app.

## Test inventory

### Seed handling and derivation (T-S1..T-S3)

| ID | Test | Category | Status |
|---|---|---|---|
| R-D1 | `valid_24_word_seed_validates` | C1 | ✅ in `derivation.rs` |
| R-D2 | `wrong_word_count_rejected` (23 words) | C1 | ✅ in `derivation.rs` |
| R-D3 | `non_bip39_word_rejected` | C1 | ✅ in `derivation.rs` |
| R-D4 | `words_with_whitespace_padding_validated` | C1 | ✅ in `derivation.rs` |
| R-D5 | Empty seed string rejected | C1 | ➕ added in this branch |
| R-D6 | Whitespace-only seed rejected | C1 | ➕ added in this branch |
| R-D7 | Seed with embedded ASCII control characters rejected | C1 | ➕ added in this branch |
| R-D8 | Very long input (1MB random bytes) rejected without panic | C1 | ➕ added in this branch |
| R-D9 | 24 valid words with deliberately broken checksum rejected | C1 | ➕ added in this branch |
| R-D10 | Unicode garbage rejected without panic | C1 | ➕ added in this branch |

### Address validation (T-F1 / T-N2 / T-B4 — destination integrity)

| ID | Test | Category | Status |
|---|---|---|---|
| R-A1 | `valid_unified_address_accepted` | C1 | ✅ |
| R-A2 | `transparent_address_rejected` | C1 | ✅ |
| R-A3 | `sapling_address_rejected` | C1 | ✅ |
| R-A4 | `garbage_string_rejected` | C1 | ✅ |
| R-A5 | `empty_string_rejected` | C1 | ✅ |
| R-A6 | Very long random string rejected without panic | C1 | ➕ added in this branch |
| R-A7 | UA prefix in upper-case rejected (case-sensitive Bech32m) | C1 | ➕ added in this branch |
| R-A8 | UA with embedded whitespace rejected | C1 | ➕ added in this branch |
| R-A9 | ZIP-321 payment URI (`zcash:u1…`) rejected as a destination | C1 | ➕ added in this branch |

### Lightwalletd endpoint integrity (T-N1 / T-N2)

| ID | Test | Category | Status |
|---|---|---|---|
| R-N1 | `endpoint_validation_rejects_remote_plaintext_http` | C1 | ✅ |
| R-N2 | `endpoint_validation_allows_loopback_http_for_local_testing` | C1 | ✅ |
| R-N3 | `network_validation_rejects_wrong_chain` | C1 | ✅ |
| R-N4 | URL without scheme rejected | C1 | ➕ added in this branch |
| R-N5 | `ftp://` and `file://` schemes rejected | C1 | ➕ added in this branch |
| R-N6 | IPv6 loopback `[::1]` over http allowed (local testing) | C1 | ➕ added in this branch |
| R-N7 | URL with embedded credentials (`https://u:p@host:port`) — documented behaviour | C1 | ➕ added in this branch |
| R-N8 | GoAway frame mid-scan triggers reconnect, no duplicate emissions | C2 | ⏸️ deferred — needs h2-aware proxy or patched lightwalletd |
| R-N9 | Hostile compact block rejected, scan errors cleanly | C2 | ⏸️ deferred — needs FakeLightwalletd gRPC fixture |
| R-N10 | All configured endpoints unreachable surfaces a clean error | C2 | 🔲 stub |
| R-N11 | TLS handshake failure surfaced without falling back to plaintext | C2 | 🔲 stub |
| R-N12 | Multi-endpoint fallback respects order with one slow endpoint | C2 | 🔲 stub |

### Workspace integrity (T-L1 / T-L3 / T-L4)

| ID | Test | Category | Status |
|---|---|---|---|
| R-W1..R-W20 | Keying isolation + metadata round-trips | C1 | ✅ 20 tests in `workspace.rs` |
| R-W21 | Created workspace directory has mode `0o700` (Unix) | C1 | ➕ added in this branch |
| R-W22 | Wallet DB files have mode `0o600` (Unix) | C1 | ➕ added in this branch |
| R-W23 | Workspace path with unicode characters round-trips correctly | C1 | ➕ added in this branch |
| R-W24 | Two argos instances on same workspace — first cancels, second proceeds | C2 | 🔲 stub |
| R-W25 | Workspace deleted between scan and sweep — clean error | C2 | 🔲 stub |
| R-W26 | Workspace permissions tampered (chmod 0444) — clean error | C2 | 🔲 stub |

### Scan-side resilience (T-N3 / T-N4 / T-N5)

| ID | Test | Category | Status |
|---|---|---|---|
| R-S1..R-S20 | Gap-limit triggering + discovery dedup | C1 | ✅ 20 tests in `scan.rs` |
| R-S21 | Birthday=0 normalised to Sapling activation | C1 | ➕ added in this branch |
| R-S22 | Birthday far above chain tip rejected | C1 | ➕ added in this branch |
| R-S23 | `gap_limit` 0/1/500/501 boundary in `validate_scan_config` | C1 | ➕ added in this branch |
| R-S24 | `num_accounts` 0/1/500/501 boundary | C1 | ➕ added in this branch |
| R-S26 | Reorg during scan invalidates and re-scans the reorganised range | C2 | ✅ implemented |
| R-S27 | Crash mid-scan — resume picks up from `fully_scanned_height` | C2 | ✅ implemented (subprocess SIGKILL of `argos-scan-helper` past block 50, resume run must reach baseline `total_zatoshis`) |
| R-S28 | Machine sleep during scan surfaces `sleep_event` | C2 | 🔲 stub (manual on a real machine — laptop lid close) |
| R-S29 | Crash mid-broadcast — resume detects broadcast tx in wallet DB | C2 | ✅ implemented (setup.sh funds 2 accounts; `argos-sweep-helper` with `--pause-millis-between-broadcasts 30000` sleeps in the gap; SIGKILL; resume run produces exactly 1 broadcast, proving no double-spend) |

### Long-running scan behaviour (T-N4 / G2)

| ID | Test | Category | Status |
|---|---|---|---|
| R-L1 | Full mainnet scan from Sapling activation to tip | C3 manual | Checklist below |
| R-L2 | Scan crosses sandblasting era — `in_sandblasting_zone` flag toggles | C3 manual | Checklist below |
| R-L3 | Scan with `gap_limit = 500` derives all 500 accounts | C3 manual | Checklist below |
| R-L4 | ETA-tracker behaviour mid-batch (no rate updates between commits) | C3 manual | Checklist below |

### Birthday subsystem (already has C1 coverage; one gap)

| ID | Test | Category | Status |
|---|---|---|---|
| R-B1..R-B12 | Date↔height conversion edge cases | C1 | ✅ 12 tests in `birthday.rs` |
| R-B13 | Auto-detect birthday probe surfaces FVK-derived addresses to server — privacy implication exposed in the result | C1 | ➕ added in this branch |

## Manual checklists

### C3 — testnet smoke flow

Run before any release candidate.

- [ ] Scan a known funded testnet seed end-to-end; reach the sweep proposal screen.
- [ ] Verify proposal numbers match a `zcash-cli`/`zebrad` query against the same addresses.
- [ ] Sweep to a testnet UA; confirm the txid on-chain.
- [ ] Restart the GUI mid-scan, re-open with the same seed/destination/birthday; confirm resume.
- [ ] Disconnect network mid-scan for 30s, reconnect; confirm scan resumes without re-scanning blocks already covered.
- [ ] Close the GUI lid (laptop sleep) for 1 minute mid-scan; confirm `sleep_event` populated when resumed.
- [ ] Configure two lightwalletd endpoints, the first deliberately unreachable; verify fallback to the second.

### C4 — mainnet small-amount gate

Required before tagging `v0.1.0`.

- [ ] Recover a seed holding 0.01 ZEC or less. Sweep to a UA we control. Confirm txid on a block explorer.
- [ ] Repeat with `--donation-rate 0.10` set (sweep + donation tests on PR #66 cover the units; this is the integration gate).
- [ ] Decrypt the donation memo with the donation address's viewing key — confirms the off-chain receipt pipeline is real.

## Regtest harness (C2)

The C2 integration tests need a Zcash regtest node and a funded regtest seed.
Out of scope for this PR — the stubs in `crates/zeck-core/tests/regtest_integration.rs`
mark their preconditions explicitly with `#[ignore]` and require:

- A running `zcashd` or `zebrad` in regtest mode listening on a known port.
- A `lightwalletd` instance pointed at that node.
- An environment variable `ARGOS_REGTEST_LIGHTWALLETD_URL` set to the
  `https://localhost:PORT` endpoint.
- A funded seed available in the regtest node — generated and miner-funded
  by a helper script (out of scope for this PR; a separate "regtest harness"
  follow-up sets that up).

When all three are present, run:

```bash
cargo test --workspace --features argos-network -- --ignored
```

CI deliberately does not run these — boot cost is too high for per-PR turnaround.

## Out of scope here

Everything related to sweep + donation lives on PR #66 and is not duplicated
in this plan. See the test counts there (121 passing as of `19731e3`).

Threat-model items that have no automatable test today and stay manual /
documented:

- T-N6 (sweep on-chain consolidation pattern) — inherent to recovery, not testable.
- T-S4 (clipboard leak surface) — bounded by the user's OS clipboard manager.
- T-SC* (supply-chain) — covered by CI tooling (cargo-deny / cargo-vet / zizmor).
- T-SC4 (Rust toolchain compromise) — out of practical reach.
- T-SC6 (SLSA provenance) — landed via PR #71.
