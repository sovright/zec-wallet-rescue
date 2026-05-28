# Argos

Argos is a recovery workspace for legacy ZecWallet Lite seeds.

This repository now contains:

- `crates/argos-core`: shared Rust library for seed validation, address derivation, lightwalletd probing, and recovery session orchestration.
- `crates/argos-cli`: a terminal interface for showing keys, scanning derived accounts, and preparing sweep requests.
- `gui/`: a Tauri v2 desktop shell with a step-by-step recovery wizard.

## Current Status

Argos now includes the major recovery phases end to end:

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
- On Unix platforms, Argos creates recovery workspace directories with private `0700` permissions and wallet/cache database files with `0600` permissions.
- Transparent funds are imported into the wallet workspace using Argos's audited legacy derivation, not modern per-account transparent derivation.
- Public lightwalletd servers learn scan metadata such as requested block ranges. Use your own lightwalletd or a local privacy proxy when that metadata matters.
- Custom lightwalletd endpoints must use HTTPS unless they target localhost/loopback for local testing.
- Broadcasted transactions are polled for confirmation during a bounded wait window. If they are still unmined at the end of that window, Argos reports them as pending instead of pretending they confirmed.
- Sprout recovery is still out of scope for seed-only flows because ZecWallet Lite did not derive Sprout keys from the HD seed.
- The GUI defaults to auto gap-limit mode and can switch to an explicit account count when the user wants an exact scan depth.
- The desktop complete screen can save a plain-text recovery report inside the persisted workspace.

## Workspace

```text
.
├── crates/
│   ├── argos-core/
│   └── argos-cli/
├── gui/
│   ├── src/
│   └── src-tauri/
└── ZECK_WALLET_LIGHT_RECOVERY_SPEC.md
```

## Installing on Windows

**The prebuilt installer is the supported Windows path.** Download the `.exe` from the [Releases page](https://github.com/sovright/zec-wallet-rescue/releases), verify the SHA256 checksum published alongside it, and run the installer. No additional dependencies are required.

Building from source is an advanced/auditor path and is supported on **Windows x64** only. Windows on ARM (aarch64-pc-windows-msvc) is not currently a supported build target; ARM64 Windows users should use the prebuilt binary.

Security note: for a wallet-recovery tool, the ability to audit and build from source is a trust property. If you are relying on the prebuilt binary, verify the published checksum before running it.

## Verifying release provenance

Each tagged release publishes a [SLSA Level 3](https://slsa.dev/spec/v1.0/levels#build-l3) in-toto provenance attestation (`argos-<tag>.intoto.jsonl`) alongside the binaries. The attestation is Sigstore-signed (no long-lived key — anchored to the GitHub Actions OIDC identity of this repository) and proves that the exact bytes of a release artifact were produced by `.github/workflows/release.yml` at the tagged commit. It complements the SHA256 checksum: the checksum proves "this file matches the one we published," and the provenance attestation proves "we published it from this source at this tag."

To verify a downloaded artifact:

```bash
# Install the verifier once (a small Go binary maintained by the SLSA project).
go install github.com/slsa-framework/slsa-verifier/v2/cli/slsa-verifier@latest

# Verify any release artifact against its provenance attestation.
slsa-verifier verify-artifact <downloaded-file> \
    --provenance-path argos-<tag>.intoto.jsonl \
    --source-uri github.com/sovright/zec-wallet-rescue \
    --source-tag <tag>
```

A passing verification confirms that `<downloaded-file>` was produced by the named tag's release workflow in this repository. macOS bundles are also code-signed (Apple Developer ID); Windows code-signing is in progress. Until the Windows certificate is provisioned, the provenance attestation is the third-party-verifiable source-to-binary chain for Windows installers — and once both are in place it complements the platform code-signing by anchoring it to a specific source-tree commit rather than only to a signing identity.

## Development

```bash
cargo fmt
cargo test -p argos-core
cargo check --workspace
cd gui && npm install
cd gui && npm run build
```
