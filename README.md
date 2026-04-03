# ZECK

ZECK is a recovery workspace for legacy ZecWallet Lite seeds.

This repository now contains:

- `crates/zeck-core`: shared Rust library for seed validation, address derivation, lightwalletd probing, and recovery session orchestration.
- `crates/zeck-cli`: a terminal interface for showing keys, scanning derived accounts, and preparing sweep requests.
- `gui/`: a Tauri v2 desktop shell with a step-by-step recovery wizard.

## Current Status

ZECK now includes the major recovery phases end to end:

- BIP-39 validation and normalization
- ZecWallet Lite-compatible Sapling, Orchard, and transparent derivation
- Unified-address destination validation
- Persisted recovery workspaces under `--data-dir` / the GUI workspace directory field
- `zcash_client_sqlite`-backed compact-block sync for authoritative transparent, Sapling, and Orchard balances
- Shared scan/sweep command surface for the CLI and GUI
- Real sweep planning with ZIP 317 fee estimation, memo support, and max-fee guards
- Real shielding/sweep transaction construction and broadcast through `lightwalletd`
- lightwalletd endpoint fallback, using comma-separated server URLs tried in order
- Progress metadata for elapsed time and ETA, plus GUI discovery notifications and recovery-report export

## Operational Notes

- Recovery sessions persist wallet/cache state on disk so repeated scans can reuse the same workspace.
- Transparent funds are imported into the wallet workspace using ZECK's audited legacy derivation, not modern per-account transparent derivation.
- Broadcasted transactions are polled for confirmation during a bounded wait window. If they are still unmined at the end of that window, ZECK reports them as pending instead of pretending they confirmed.
- Sprout recovery is still out of scope for seed-only flows because ZecWallet Lite did not derive Sprout keys from the HD seed.
- The GUI defaults to auto gap-limit mode and can switch to an explicit account count when the user wants an exact scan depth.
- The desktop complete screen can now save a plain-text recovery report inside the persisted workspace or any other chosen path.

## Workspace

```text
.
├── crates/
│   ├── zeck-core/
│   └── zeck-cli/
├── gui/
│   ├── src/
│   └── src-tauri/
└── ZECK_WALLET_LIGHT_RECOVERY_SPEC.md
```

## Development

```bash
cargo fmt
cargo test -p zeck-core
cargo check --workspace
cd gui && npm install
cd gui && npm run build
```
