#!/usr/bin/env bash
# Argos regtest harness — initial-funding script.
#
# Run AFTER `docker compose up -d` has the stack healthy. Mines past coinbase
# maturity (200 blocks), then funds the well-known Argos test seed's first
# transparent address with a deterministic amount so the C2 integration tests
# have a known-funded wallet to recover.
#
# Exits non-zero on any failure; safe to re-run (idempotent — checks the
# block height and the test address's UTXO set before re-mining/re-sending).
#
# Prerequisites:
#   - `docker compose up -d` running in this directory.
#   - `argos-cli` on PATH (or set $ARGOS_CLI to its path) — used to derive
#     the test seed's transparent address. Build with
#         cargo build -p argos-cli --release
#     and point `ARGOS_CLI` at `target/release/argos`.
#   - `jq` for parsing zcash-cli JSON output.
#
# Environment overrides:
#   ARGOS_CLI                  Path to the argos binary (default: `argos`).
#   REGTEST_INITIAL_BLOCKS     How many blocks to ensure are mined before
#                              funding (default 200; bumps coinbase out of
#                              maturity, see zcashd's COINBASE_MATURITY=100
#                              and the 100-block fee shielding window).
#   REGTEST_FUND_ZEC           How much ZEC to send to the test seed's
#                              transparent address (default 5).
#   REGTEST_ZCASHD_CONTAINER   Container name (default argos-zcashd-regtest).

set -euo pipefail

readonly ARGOS_CLI="${ARGOS_CLI:-argos}"
readonly INITIAL_BLOCKS="${REGTEST_INITIAL_BLOCKS:-200}"
readonly FUND_ZEC="${REGTEST_FUND_ZEC:-5}"
readonly ZCASHD="${REGTEST_ZCASHD_CONTAINER:-argos-zcashd-regtest}"
# Argos test seed (BIP-39 test vector — no real funds anywhere). Documented
# in CLAUDE.md as the only seed safe to commit anywhere.
readonly ARGOS_TEST_SEED="abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art"

log() { printf '[regtest-setup] %s\n' "$*"; }
die() { printf '[regtest-setup] ERROR: %s\n' "$*" >&2; exit 1; }

zcli() {
    docker exec "$ZCASHD" zcash-cli -conf=/srv/zcashd/.zcash/zcash.conf "$@"
}

# ── Prerequisites ───────────────────────────────────────────────────────────

command -v docker >/dev/null 2>&1 || die "docker not found on PATH"
command -v jq     >/dev/null 2>&1 || die "jq not found on PATH"
command -v "$ARGOS_CLI" >/dev/null 2>&1 \
    || die "$ARGOS_CLI not on PATH — set ARGOS_CLI=path/to/argos or cargo build -p argos-cli --release"

docker ps --format '{{.Names}}' | grep -qx "$ZCASHD" \
    || die "container $ZCASHD is not running — run 'docker compose up -d' first"

# ── Wait for RPC readiness ─────────────────────────────────────────────────

log "waiting for zcashd RPC to respond..."
for i in $(seq 1 30); do
    if zcli getblockchaininfo >/dev/null 2>&1; then
        log "RPC ready"
        break
    fi
    sleep 2
done
zcli getblockchaininfo >/dev/null \
    || die "zcashd RPC never came up — check 'docker compose logs zcashd-regtest'"

# ── Derive the test seed's transparent address via argos-cli ───────────────
#
# We rely on argos's own derivation so the address is exactly what the test
# wallet expects to see. `argos show-keys` prints account 0's
# transparent receive address on the first line of the relevant section;
# the awk grabs it. The test seed has a well-known result so the address is
# verifiable by hand if needed.

log "deriving test seed's transparent address via $ARGOS_CLI..."
# Run on testnet/mainnet doesn't matter — the BIP-44 transparent path uses
# coin_type=133 only for mainnet; regtest uses coin_type=1 just like testnet.
# Argos doesn't know about regtest, but the test seed's testnet t-addr
# matches what regtest accepts (regtest uses testnet's P2PKH version bytes).
TEST_T_ADDR="$(
    ARGOS_SEED="$ARGOS_TEST_SEED" "$ARGOS_CLI" show-keys \
        --seed-env-var ARGOS_SEED \
        --network testnet \
        --num-accounts 1 \
    | awk '/transparent.*receive/{found=1; next} found && /^  [tm]/{print $1; exit}'
)"
[ -n "$TEST_T_ADDR" ] \
    || die "could not extract transparent address from argos-cli show-keys output"
log "test seed transparent address: $TEST_T_ADDR"

# ── Mine to maturity ───────────────────────────────────────────────────────

CURRENT_HEIGHT="$(zcli getblockcount)"
if [ "$CURRENT_HEIGHT" -lt "$INITIAL_BLOCKS" ]; then
    NEEDED=$(( INITIAL_BLOCKS - CURRENT_HEIGHT ))
    log "current height $CURRENT_HEIGHT < $INITIAL_BLOCKS; mining $NEEDED blocks..."
    zcli generate "$NEEDED" >/dev/null
    log "mined to height $(zcli getblockcount)"
else
    log "current height $CURRENT_HEIGHT >= $INITIAL_BLOCKS — skipping initial mining"
fi

# ── Idempotency: if the test address already has UTXOs, skip funding ───────

EXISTING_BAL="$(zcli getreceivedbyaddress "$TEST_T_ADDR" 0 || echo "0")"
EXISTING_BAL="${EXISTING_BAL%.*}" # drop fractional part for the comparison
if [ "${EXISTING_BAL:-0}" -gt 0 ] 2>/dev/null; then
    log "test address already funded ($EXISTING_BAL+ ZEC); skipping send"
else
    log "sending $FUND_ZEC ZEC to $TEST_T_ADDR..."
    TXID="$(zcli sendtoaddress "$TEST_T_ADDR" "$FUND_ZEC")"
    log "funding txid: $TXID"
    log "mining 1 block to confirm..."
    zcli generate 1 >/dev/null
fi

# ── Summary ────────────────────────────────────────────────────────────────

HEIGHT="$(zcli getblockcount)"
BAL="$(zcli getreceivedbyaddress "$TEST_T_ADDR" 1)"
cat <<EOF

[regtest-setup] DONE.
  height:             $HEIGHT
  test seed t-addr:   $TEST_T_ADDR
  confirmed balance:  $BAL ZEC

Export for tests:
    export ARGOS_REGTEST_LIGHTWALLETD_URL=http://localhost:9067
    export ARGOS_REGTEST_TEST_T_ADDR=$TEST_T_ADDR

Then run:
    cargo test --workspace -- --ignored

EOF
