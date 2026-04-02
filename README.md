# ZECK

ZECK is a recovery workspace for legacy ZecWallet Lite seeds.

This repository now contains:

- `crates/zeck-core`: shared Rust library for seed validation, address derivation, lightwalletd probing, and recovery session orchestration.
- `crates/zeck-cli`: a terminal interface for showing keys, previewing recovery sessions, and preparing sweep requests.
- `gui/`: a Tauri v2 desktop shell with a step-by-step recovery wizard.

## Current Status

This first implementation pass focuses on the architecture and the safety-sensitive parts that can be made authoritative immediately:

- BIP-39 validation and normalization
- ZecWallet Lite-compatible Sapling, Orchard, and transparent derivation
- Unified-address destination validation
- lightwalletd connectivity probing
- Shared scan/sweep command surface for the CLI and GUI

The chain scan and broadcast steps are intentionally surfaced as preview-oriented workflow pieces for now, with explicit warnings where compact-block sync and on-chain sweeping still need to be wired into the same public interfaces.

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
cargo check
cd gui && npm install
```
