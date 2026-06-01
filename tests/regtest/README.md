# Argos regtest harness

A Docker-based local Zcash regtest stack for the C2 integration tests defined
in `crates/zeck-core/tests/regtest_integration.rs`. These tests exercise the
fund-recovery flow against a real Zcash node — scan, sweep, broadcast, resume
— in scenarios that aren't reachable from pure unit tests (GoAway frames mid
scan, hostile compact blocks, mid-scan crashes, reorgs, etc.).

The harness is **opt-in** and **never runs in CI**:

- The integration tests are tagged `#[ignore]` so default `cargo test` skips them.
- Booting a regtest stack on every PR would inflate CI runtime past the cache
  TTL and is not worth it for tests this rarely fail.
- Contributors run this locally before merging changes to scan/sweep logic.

## What boots

```
zcashd-regtest      Private Zcash network, listens on 127.0.0.1:18232 (RPC)
lightwalletd-regtest gRPC server pointing at zcashd, on 127.0.0.1:9067 (no TLS)
```

Both bound to **loopback only**. Never expose these ports — the regtest RPC
credentials are well-known and a remote miner with control of those ports
can mint regtest coins indefinitely.

## Prerequisites

- **Docker** (Docker Desktop on macOS / Windows; native `docker` + `docker
  compose` plugin on Linux). Tested with Docker 24+.
- **`jq`** for the setup script's JSON parsing (`brew install jq` /
  `apt-get install jq`).
- **`argos-cli`** built. The setup script derives the test seed's
  transparent address using `argos show-keys`, so the binary must be on
  `PATH` or pointed to via `$ARGOS_CLI`:

  ```bash
  cargo build -p argos-cli --release
  export ARGOS_CLI="$(pwd)/target/release/argos"
  ```

## One-time setup

From the **repository root**:

```bash
cd tests/regtest

# Boot the stack (-d = detached).
docker compose up -d

# Wait for the healthcheck to pass and run the funding script.
# This mines 200 blocks (clears coinbase maturity) and sends 5 ZEC each to
# accounts 0 and 1 of the Argos test seed's transparent addresses (2 funded
# accounts by default — R-S29 requires multiple per-account broadcasts).
# Override the account count with REGTEST_FUND_ACCOUNTS=N. Idempotent —
# safe to re-run.
./setup.sh
```

The script prints the lightwalletd URL and one funded transparent address
per derived account. Export them as the integration tests expect:

```bash
export ARGOS_REGTEST_LIGHTWALLETD_URL=http://localhost:9067
export ARGOS_REGTEST_TEST_T_ADDR=t1...     # account 0; printed by setup.sh
export ARGOS_REGTEST_TEST_T_ADDR_0=t1...   # same as above; one-per-account form
export ARGOS_REGTEST_TEST_T_ADDR_1=t1...   # account 1; required by R-S29
```

## Running the integration tests

From the **repository root**:

```bash
cargo test --workspace --features argos-network -- --ignored
```

Both flags are required:

- `--features argos-network` enables the `argos-core` feature that teaches
  `validate_lightwalletd_network` to accept the regtest chain name and
  skip the testnet Sapling-activation-height check. Production builds
  compile this out so a hostile mainnet lightwalletd cannot claim to be
  regtest to bypass network validation in a released Argos binary.
- `--ignored` runs the `#[ignore]`-tagged C2 tests; default `cargo test`
  still skips them.

Without `--features argos-network`, the integration test file is gated out
by `#![cfg(feature = "argos-network")]` and compiles to an empty test
binary. CI runs the default form only.

Each integration test prints a `[regtest]` header noting the harness URL it
connected to, so a mid-test failure is easy to attribute to the stack vs to
Argos logic.

## Teardown

```bash
cd tests/regtest
docker compose down -v        # -v wipes the named volumes too
```

Without `-v`, the named volumes (`zcashd-data`, `lwd-data`) persist between
runs, so the chain state survives a `down`/`up`. The setup script is
idempotent against an existing chain, so you only need `-v` if you want a
fresh chain (e.g. to exercise a clean-slate test).

## What this harness is not

- **It is not a fuzzing harness.** Fault injection (GoAway frames, malformed
  compact blocks, TLS-handshake failure, etc.) needs server-side cooperation
  — typically a custom lightwalletd build or a Mitm proxy. Those stubs in
  `crates/zeck-core/tests/regtest_integration.rs` will need additional
  scaffolding before their bodies can be implemented; the harness here just
  provides the baseline "two healthy services + a funded seed" foundation.
- **It is not for cross-platform verification.** docker-compose runs
  Linux containers regardless of host. macOS users still get the right
  test outcome but the in-container OS is Linux.
- **It is not a replacement for the manual C3 testnet smoke flow** documented
  in `docs/superpowers/test-plans/recovery-resilience.md`. Regtest validates
  Argos against a deterministic toy chain; testnet validates against the real
  Zcash p2p network and the public lightwalletd operators we depend on.

## Bare-metal alternative

If you already have `zcashd` + `lightwalletd` installed locally (e.g. for
zebra contributors), you can skip docker entirely. The integration tests
only care about `ARGOS_REGTEST_LIGHTWALLETD_URL` and a funded test seed.
Configure your local zcashd with the equivalent of `zcashd-regtest.conf`,
boot lightwalletd against it, mine + fund manually, then export the URL and
run `cargo test --workspace --features argos-network -- --ignored`.

## Status of the tests

12 integration tests are stubbed in `crates/zeck-core/tests/regtest_integration.rs`
as `#[ignore]` with `unimplemented!()` bodies. Each documents what it would
verify against a running harness. Implementing them is the **next** focused
PR after this scaffolding lands.
