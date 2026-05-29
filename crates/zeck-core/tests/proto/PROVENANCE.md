# Vendored lightwalletd proto provenance

The two `.proto` files in this directory are derived from the upstream
[zcash/lightwalletd](https://github.com/zcash/lightwalletd) repository.

## Source commit

- Repository: `https://github.com/zcash/lightwalletd`
- Branch: `master`
- Commit: `45b2f4cdd4aeb51f0683d43e96a04b5f8a4af683`
- Pulled: 2026-05-28

## Files

### `compact_formats.proto`

Vendored verbatim from `walletrpc/compact_formats.proto` at the commit above.
No modifications.

### `service.proto`

Derived from `walletrpc/service.proto` at the commit above. **Pruned** — only
the RPC methods Argos's gRPC client invokes, and their transitively-referenced
messages, are retained. The wire-format of every retained message is byte-for-
byte identical to upstream.

## Why these files are vendored

The Argos test fixture (`tests/common/fake_lightwalletd.rs`) needs a generated
gRPC **server** stub. Argos's runtime client uses the gRPC client stubs already
generated inside `zcash_client_backend::proto`, but that crate does not export
the server stubs (and shouldn't — they would just be dead code in production).
Rather than depending on `zcash_client_backend`'s internal build script, we
compile our own copy of the protos under `argos-network`-gated `tests/proto/`.

The `cash.z.wallet.sdk.rpc` package and every field number, type, and tag is
preserved exactly, so the fixture is wire-compatible with the real
lightwalletd protocol and with `zcash_client_backend`'s client.

## RPCs retained

- `GetBlock`
- `GetBlockRange`
- `GetTransaction`
- `SendTransaction`
- `GetTaddressTxids`
- `GetTreeState`
- `GetAddressUtxos`
- `GetLightdInfo`

## RPCs intentionally dropped

The following upstream RPCs are not in the pruned service definition because
Argos does not call them as of the commit on this branch. If Argos starts
using one, restore the RPC in `service.proto` and implement it in the fixture
service.

- `GetLatestBlock`
- `GetBlockNullifiers` (deprecated upstream)
- `GetBlockRangeNullifiers` (deprecated upstream)
- `GetTaddressTransactions`
- `GetTaddressBalance` / `GetTaddressBalanceStream`
- `GetMempoolTx` / `GetMempoolStream`
- `GetLatestTreeState`
- `GetSubtreeRoots`
- `GetAddressUtxosStream`
- `Ping`

## How to refresh

```sh
git clone --depth 1 https://github.com/zcash/lightwalletd.git /tmp/lwd
cp /tmp/lwd/walletrpc/compact_formats.proto crates/zeck-core/tests/proto/
# Re-prune walletrpc/service.proto by hand to the methods listed above,
# preserving every field number and tag exactly.
git -C /tmp/lwd rev-parse HEAD  # update the commit hash above
```
