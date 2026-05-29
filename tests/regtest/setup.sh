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
#   REGTEST_FUND_ZEC           How much ZEC to send to EACH funded account's
#                              transparent address (default 5).
#   REGTEST_FUND_ACCOUNTS      How many accounts to derive and fund
#                              (default 2). R-S29 needs at least 2 so a sweep
#                              produces multiple per-account broadcasts;
#                              R-S27 is satisfied with 1. Defaulting to 2
#                              keeps the harness ready for both without a
#                              second invocation.
#   REGTEST_ZCASHD_CONTAINER   Container name (default argos-zcashd-regtest).

set -euo pipefail

readonly ARGOS_CLI="${ARGOS_CLI:-argos}"
readonly INITIAL_BLOCKS="${REGTEST_INITIAL_BLOCKS:-200}"
readonly FUND_ZEC="${REGTEST_FUND_ZEC:-5}"
readonly FUND_ACCOUNTS="${REGTEST_FUND_ACCOUNTS:-2}"
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

# ── Derive the test seed's transparent address(es) via argos-cli ───────────
#
# We rely on argos's own derivation so the addresses are exactly what the
# test wallet expects to see. `argos show-keys` prints a `Transparent
# receive <addr>  (<path>)` line per derived account; the awk grabs the
# address column. The test seed has a well-known result so the addresses are
# verifiable by hand if needed.

log "deriving test seed's transparent addresses (accounts 0..$((FUND_ACCOUNTS - 1))) via $ARGOS_CLI..."
# Run on testnet/mainnet doesn't matter — the BIP-44 transparent path uses
# coin_type=133 only for mainnet; regtest uses coin_type=1 just like testnet.
# Argos doesn't know about regtest, but the test seed's testnet t-addr
# matches what regtest accepts (regtest uses testnet's P2PKH version bytes).
mapfile -t TEST_T_ADDRS < <(
    ARGOS_SEED="$ARGOS_TEST_SEED" "$ARGOS_CLI" show-keys \
        --seed-env-var ARGOS_SEED \
        --network testnet \
        --num-accounts "$FUND_ACCOUNTS" \
    | awk '/^  Transparent receive /{print $3}'
)

[ "${#TEST_T_ADDRS[@]}" -eq "$FUND_ACCOUNTS" ] \
    || die "expected $FUND_ACCOUNTS transparent addresses from argos show-keys, got ${#TEST_T_ADDRS[@]}"

for i in "${!TEST_T_ADDRS[@]}"; do
    log "  account $i t-addr: ${TEST_T_ADDRS[$i]}"
done

# Preserve the legacy single-address name so any caller still reading
# $TEST_T_ADDR keeps working — it just points at account 0.
TEST_T_ADDR="${TEST_T_ADDRS[0]}"

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

# ── Idempotency: fund each derived account that isn't already funded ───────

NEW_TXIDS=()
for i in "${!TEST_T_ADDRS[@]}"; do
    addr="${TEST_T_ADDRS[$i]}"
    existing="$(zcli getreceivedbyaddress "$addr" 0 || echo "0")"
    existing_int="${existing%.*}"
    if [ "${existing_int:-0}" -gt 0 ] 2>/dev/null; then
        log "account $i already funded (${existing}+ ZEC); skipping send"
    else
        log "sending $FUND_ZEC ZEC to account $i ($addr)..."
        txid="$(zcli sendtoaddress "$addr" "$FUND_ZEC")"
        log "  funding txid: $txid"
        NEW_TXIDS+=("$txid")
    fi
done

if [ "${#NEW_TXIDS[@]}" -gt 0 ]; then
    log "mining 1 block to confirm ${#NEW_TXIDS[@]} new send(s)..."
    zcli generate 1 >/dev/null
fi

# ── Summary ────────────────────────────────────────────────────────────────

HEIGHT="$(zcli getblockcount)"
cat <<EOF

[regtest-setup] DONE.
  height:             $HEIGHT
EOF
for i in "${!TEST_T_ADDRS[@]}"; do
    addr="${TEST_T_ADDRS[$i]}"
    bal="$(zcli getreceivedbyaddress "$addr" 1)"
    printf '  account %d t-addr:   %s (%s ZEC confirmed)\n' "$i" "$addr" "$bal"
done
cat <<EOF

Export for tests:
    export ARGOS_REGTEST_LIGHTWALLETD_URL=http://localhost:9067
    export ARGOS_REGTEST_TEST_T_ADDR=$TEST_T_ADDR
EOF
# Per-account env vars for R-S29 and any future multi-account test.
for i in "${!TEST_T_ADDRS[@]}"; do
    printf '    export ARGOS_REGTEST_TEST_T_ADDR_%d=%s\n' "$i" "${TEST_T_ADDRS[$i]}"
done
cat <<EOF

Then run:
    cargo test --workspace -- --ignored
    # or, for the C2 suite that includes R-S27/R-S29/R-N8/R-N9:
    cargo test --workspace --features argos-network -- --ignored

EOF
