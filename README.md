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
- Persisted recovery workspaces under random per-session subdirectories of `--data-dir` / the GUI workspace directory field
- `zcash_client_sqlite`-backed compact-block sync for authoritative transparent, Sapling, and Orchard balances
- Shared scan/sweep command surface for the CLI and GUI
- Real sweep planning with ZIP 317 fee estimation, memo support, and max-fee guards
- Real shielding/sweep transaction construction and broadcast through `lightwalletd`
- lightwalletd endpoint fallback, using comma-separated server URLs tried in order
- Progress metadata for elapsed time and ETA, plus GUI discovery notifications and recovery-report export

## Operational Notes

- Recovery sessions persist wallet/cache state on disk for auditability and sweep construction. Workspace subdirectories are random per session so the path does not reveal a stable seed fingerprint.
- On Unix platforms, ZECK creates recovery workspace directories with private `0700` permissions and wallet/cache database files with `0600` permissions.
- Transparent funds are imported into the wallet workspace using ZECK's audited legacy derivation, not modern per-account transparent derivation.
- Public lightwalletd servers learn scan metadata such as requested block ranges. Use your own lightwalletd or a local privacy proxy when that metadata matters.
- Custom lightwalletd endpoints must use HTTPS unless they target localhost/loopback for local testing.
- Broadcasted transactions are polled for confirmation during a bounded wait window. If they are still unmined at the end of that window, ZECK reports them as pending instead of pretending they confirmed.
- Sprout recovery is still out of scope for seed-only flows because ZecWallet Lite did not derive Sprout keys from the HD seed.
- The GUI defaults to auto gap-limit mode and can switch to an explicit account count when the user wants an exact scan depth.
- The desktop complete screen can save a plain-text recovery report inside the persisted workspace.

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

## Installing on Windows

**The prebuilt installer is the supported Windows path.** Download the `.exe` from the [Releases page](https://github.com/sovright/zec-wallet-rescue/releases), verify the SHA256 checksum published alongside it, and run the installer. No additional dependencies are required.

Building from source is an advanced/auditor path and is supported on **Windows x64** only. Windows on ARM (aarch64-pc-windows-msvc) is not currently a supported build target; ARM64 Windows users should use the prebuilt binary.

Security note: for a wallet-recovery tool, the ability to audit and build from source is a trust property. If you are relying on the prebuilt binary, verify the published checksum before running it.

## Development

```bash
cargo fmt
cargo test -p zeck-core
cargo check --workspace
cd gui && npm install
cd gui && npm run build
```
