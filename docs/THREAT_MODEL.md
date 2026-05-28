# Argos Threat Model

> **Status:** Initial draft, v0.1.0-rc. This document describes the security posture of Argos as of the v0.1.0 release candidate. It is a living document; revisit on every release and whenever a new attack surface is added.

## 1. Purpose and scope

Argos is a single-use Zcash wallet **recovery** tool for ZecWallet Lite seeds. Its purpose is to take a 24-word BIP-39 seed phrase, scan the Zcash chain for funds derived under ZecWallet Lite's account layout, and sweep them to a modern wallet (Zashi/Zodl, YWallet) in a single session. It is not an everyday wallet.

This threat model covers:

- the desktop GUI (Tauri v2: HTML/CSS/JS frontend in WebView2/WKWebView/WebKitGTK + Rust backend)
- the CLI (`argos-cli`)
- the shared core library (`argos-core`)
- the build / release / distribution pipeline (GitHub Actions, Vercel marketing site, signed installers)

It does **not** cover the security of the user's host operating system, the user's destination wallet, the lightwalletd nodes operated by third parties, or the Zcash consensus protocol itself.

## 2. System overview

### 2.1 Components

| Component | Process | Language | Trust boundary |
|---|---|---|---|
| `argos-gui` (Tauri shell + WebView) | 2 processes (Rust host + WebView renderer) | Rust + HTML/CSS/JS | Host process trusts WebView only via `invoke` IPC; WebView is sandboxed by the OS |
| `argos-cli` | 1 process | Rust | Inherits the user's shell trust |
| `argos-core` | (library) | Rust | — |
| lightwalletd | Remote, over TLS gRPC | Go (third party) | Untrusted network peer |
| Local workspace (SQLite) | On disk | — | Same trust as the user's home directory |

### 2.2 Data flow

```
   user input (seed, destination, config)
            │
            ▼
   ┌──────────────────┐
   │  argos-gui  /  argos-cli                     │
   │   - SecretString-wrapped seed                │
   │   - BIP-39 → seed bytes (Secret<[u8;64]>)    │
   │   - ZIP-32/legacy-transparent derivation     │
   └──────────────────┘
            │
            │  full viewing keys + spending keys (in process memory)
            ▼
   ┌──────────────────┐               TLS over HTTP/2                  ┌──────────────┐
   │ zcash_client_*    │  ◀────────  gRPC: compact blocks  ────────▶  │ lightwalletd │
   │  (sync + scan)    │             tx fetch, t-utxo                  │   (remote)    │
   └──────────────────┘                                                └──────────────┘
            │
            │  writes wallet DB (FVKs, IVKs, notes, witnesses)
            ▼
   ┌──────────────────┐
   │  workspace.sqlite  │      ←—— resume cursor across restarts
   │  blocks.sqlite      │      ←—— shared compact-block cache
   └──────────────────┘
            │
            │  Orchard/Sapling/Transparent proposals signed in-process
            ▼
   broadcast (tonic / tls) ──▶ lightwalletd ──▶ Zcash network
```

## 3. Assets

In rough priority order:

1. **The 24-word seed phrase.** Sole authority to spend any funds derivable from it.
2. **Recovered ZEC.** Sweep transactions move value from the legacy ZWL accounts to the user's chosen destination.
3. **The destination unified address.** Privacy-sensitive linkage between the user and the recovered funds.
4. **Workspace contents.** Contains full viewing keys (FVKs), incoming viewing keys (IVKs), per-account note cache, witnesses, and historic balances. With FVKs alone an attacker cannot spend, but can fully reconstruct the wallet's transaction history.
5. **The shared compact-block cache.** Public chain data; not sensitive by itself, but the *set of heights present* leaks an upper bound on which wallets have been scanned on this host.
6. **The recovery report.** Plaintext file written by the user with workspace path, txids, account labels, and net amounts.

## 4. Trust boundaries

- **User ↔ host OS:** Argos trusts the host. A compromised OS defeats every other mitigation.
- **Tauri host process ↔ WebView renderer:** The renderer can only reach the host via explicit `#[tauri::command]` handlers and is constrained by the CSP in `gui/src-tauri/tauri.conf.json` (`default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: asset: http://asset.localhost; font-src 'self'; connect-src ipc: http://ipc.localhost`). No remote script or remote `connect-src` is permitted.
- **Argos ↔ lightwalletd:** Untrusted server reachable only over TLS. Default endpoints (`zec.rocks`, `na.zec.rocks` for mainnet; `lightwalletd.testnet.electriccoin.co` for testnet) are configurable per scan.
- **Argos ↔ local disk:** Workspace directories and database files are written to a user-chosen directory (defaulting to the platform `AppDataDir/workspace`). Workspace directories are created with `0o700` and database files with `0o600` (`workspace.rs:set_private_file_permissions`). The `session.json` sidecar contains no keys — only label, network, birthday, and timestamps — and inherits the OS umask.
- **Build pipeline ↔ release artifact:** Signing keys live in GitHub Actions environments gated to protected branches (PR #48). Windows signing is **not yet implemented** (see §8).

## 5. Threat actors

| Actor | Capability | In scope? |
|---|---|---|
| Curious local user with shell access | Read files under the user's account, list processes | Yes |
| Malware running as the user | Read memory, read disk, intercept clipboard, screen-capture, keylog | Partially — we mitigate disk/clipboard exposure but cannot defeat in-process attackers |
| Malware running as a *different* unprivileged user | Read processes / files of the Argos user via OS bugs | OS-level — out of scope |
| Network observer on the local segment | Passive sniffing | Yes |
| Hostile lightwalletd operator | Serve crafted compact blocks, log query patterns | Yes |
| Hostile DNS / TLS-trust-store attacker | Substitute lightwalletd endpoint | Partially — webpki trust roots only |
| Compromised upstream Rust/JS dependency | Inject malicious code at build time | Partially — see §7 |
| Compromised GitHub Actions / signing key | Sign a malicious installer | Yes — see §8 |
| Casual shoulder-surfer | Read the screen while seed is visible | Yes |
| Coerced user ($5 wrench attack) | Forced to run a sweep under duress | Out of scope |
| Nation-state with cryptanalytic capability | Break Sapling/Orchard or post-quantum threats | Out of scope |

## 6. Threats and mitigations

Severity: **C**ritical / **H**igh / **M**edium / **L**ow. Status: ✅ mitigated, ⚠️ partial, ❌ open.

### 6.1 Secret handling

| ID | Threat | Severity | Status | Mitigation |
|---|---|---|---|---|
| T-S1 | Seed phrase remains in process memory after use; swapped to disk | H | ✅ | Seed is wrapped in `secrecy::SecretString` and the BIP-39-derived 64-byte seed in `Secret<[u8;64]>` (PR #47). `Drop` zeroizes underlying memory. We do **not** call `mlock`/`VirtualLock` — swap remains a residual risk on the host. |
| T-S2 | Seed phrase ends up in JS state and outlives the scan | H | ✅ | `state.scanConfig` stores a seed-less copy of the config (network/birthday/account params only). The seed is passed to the `start_scan` Tauri command and the textarea is cleared on submit; no JS reference outlives the call. |
| T-S3 | Seed phrase leaks via logs / tracing / `Debug` impl | H | ✅ | `secrecy` wrappers do not implement `Debug`/`Display` for their inner value. No `println!`/`tracing` calls on seed-bearing variables. |
| T-S4 | Seed phrase leaks via clipboard | M | ⚠️ | Argos never writes the seed to the clipboard. The recovery-report "Copy path" button (PR #53) only copies the file path. Users pasting their own seed in is bounded by the OS clipboard's lifetime. |
| T-S5 | Seed visible on screen during entry | L | ✅ | Seed textarea is blurred by default; user must explicitly toggle "Show words on screen". |

### 6.2 Frontend (Tauri + WebView)

| ID | Threat | Severity | Status | Mitigation |
|---|---|---|---|---|
| T-F1 | XSS via lightwalletd-controlled data rendered in the UI | H | ⚠️ | Strict CSP forbids inline/remote scripts. We rely on the DOM API (`textContent`, not `innerHTML`) for server-derived strings. Worth auditing every render path that includes an address, label, or memo. |
| T-F2 | Supply-chain attack via npm packages | M | ✅ | `withGlobalTauri: true`; `main.js` has zero imports; no `node_modules` in the runtime bundle (PR #50). |
| T-F3 | Tauri command surface broader than necessary | M | ⚠️ | All commands live in `gui/src-tauri/src/commands.rs`. Worth a periodic review to ensure each one needs to exist and validates its inputs. |
| T-F4 | localStorage leaks (e.g. dismissed-session IDs) | L | ✅ | Only non-sensitive UI state (sidebar width, dismissed-session workspace paths) lives in localStorage. No secrets. |

### 6.3 Network

| ID | Threat | Severity | Status | Mitigation |
|---|---|---|---|---|
| T-N1 | Passive observer learns user is scanning Zcash | L | ✅ | All lightwalletd traffic is TLS. Endpoint discoverable via SNI, which is expected for a public service. |
| T-N2 | Active MITM substitutes lightwalletd | H | ⚠️ | Standard webpki trust roots only — **no certificate pinning** of `zec.rocks`. A user with a poisoned trust store can have their queries (and broadcasts) routed to a hostile node. Pinning the default endpoints is tracked as a follow-up. |
| T-N3 | Hostile lightwalletd serves invalid compact blocks | M | ✅ | `zcash_client_backend::sync::run` validates witness consistency against the chain tip and rejects malformed/inconsistent blocks. |
| T-N4 | Hostile lightwalletd correlates a user's IP with their wallet | H | ⚠️ | Inherent to the lightwalletd protocol. Mitigations: configurable endpoint (run your own), the `GetAddressUtxos` quick-probe queries 10 t-addrs (5 accounts × 2 addresses: external + change) which leaks them in plaintext (post-TLS) to the server, and the compact-block scan range leaks the wallet birthday. No Tor integration. |
| T-N5 | Auto-detect probe leaks viewing-key-derived addresses | M | ⚠️ | The auto-detect flow (`crates/zeck-core/src/birthday.rs`) imports an account into a temp workspace and runs a windowed sync. This sends FVK-derived address queries to the server. Documented in the UI ("requires a server connection"), but worth surfacing more clearly. |
| T-N6 | Sweep transaction broadcast reveals consolidation pattern | M | ⚠️ | A single sweep aggregates funds from many ZWL accounts into one destination, which on-chain analysis can link. Inherent to recovery — no good mitigation without changing the sweep model. |

### 6.4 Local storage

| ID | Threat | Severity | Status | Mitigation |
|---|---|---|---|---|
| T-L1 | Other local users / processes read the workspace DB | M | ✅ | Workspace directories are created `0o700` and database files `0o600` at creation time (`workspace.rs:set_private_file_permissions`, implemented in PR #43). Workspace contains FVKs/IVKs (privacy leak) and witnesses, not the seed. `session.json` (label, network, birthday, timestamps — no keys) inherits the OS umask. |
| T-L2 | Recovery report contains sensitive metadata | L | ✅ | Report is user-initiated, written to a user-chosen path. Contents are documented in the UI before save (network, birthday, accounts, mode, workspace path, txids, net amounts). |
| T-L3 | Workspace persists indefinitely after recovery | L | ✅ | The GUI's Recovery-complete screen now exposes a "Delete workspace" action (`RecoveryService::delete_workspace` → `fs::remove_dir_all`). The UI explicitly surfaces that this is not a cryptographic wipe on SSDs — block-level remnants may persist until cells are overwritten or TRIM'd. For high-value seeds, users are directed to encrypt the volume containing the workspace. CLI users can `rm -rf` the workspace path printed at the end of a scan. |
| T-L4 | Resume-session metadata identifies prior recoveries | L | ✅ | The resume panel only shows workspaces under the configured data-dir; dismissed sessions stay dismissed via localStorage (PR #53). Sessions can be excluded without deleting on-disk state. |

### 6.5 Build, release, distribution

| ID | Threat | Severity | Status | Mitigation |
|---|---|---|---|---|
| T-B1 | Compromised cargo dependency injects code at build time | H | ✅ | We pin via `Cargo.lock`. CI runs `cargo check` + `clippy` + tests + `cargo audit` (via `rustsec/audit-check`) on every push and PR. Advisories surface as a failed job. |
| T-B2 | Compromised GitHub Actions secret signs a malicious release | C | ✅ | Signing and publish jobs are gated on protected environments (PR #48); only tagged release workflows can access signing keys. macOS signing is in place. |
| T-B3 | Windows installer is unsigned | H | ❌ | No Windows code-signing certificate has been provisioned (acknowledged in PR #54). Users currently must verify SHA256 checksums manually. Tracked. |
| T-B4 | Installer tampered with after publish | M | ✅ | SHA256 checksums are published alongside each artifact (deduplicated via PR #47/#48). README directs Windows users to verify the checksum before running the installer. |
| T-B5 | Marketing site (sovright.com / Vercel preview) ships a different binary than the release page | L | ✅ | The site does not host binaries; download links point at `github.com/sovright/zec-wallet-rescue/releases`. |

### 6.6 Supply chain integrity

The dependency tree is large (~700 transitive crates, dominated by the librustzcash stack and Tauri's GTK/WebKit shell on Linux) and almost all of the executable code in a released Argos binary comes from third-party crates. A compromise anywhere in that tree, in the build toolchain, or in CI is the single highest-impact attack class against this project. The threats below are listed separately from §6.5 because the mitigations differ: §6.5 is about *our* build and release pipeline, while §6.6 is about the integrity of the inputs that flow into it.

| ID | Threat | Severity | Status | Mitigation |
|---|---|---|---|---|
| T-SC1 | Malicious `build.rs` script or procedural macro in a transitive crate runs arbitrary code at compile time (dev machine and CI). | H | ❌ | Not currently audited. `Cargo.lock` pins the exact crate version we compile, so a published fix to upstream cannot regress us silently, but it does not prevent compromise of the version we already trust. Tracked as an open item. |
| T-SC2 | Maintainer-account takeover on a critical crate (librustzcash family, `rustls`, `tauri`, `secrecy`, `secp256k1`, `bip0039`) ships a malicious version that we knowingly bump to. | H | ⚠️ | `cargo audit` (T-B1) cannot detect a zero-day at bump time. Project policy requires conservative dependency review — see `CLAUDE.md` and `~/.claude/approved-dependencies.md` — and the README/threat model document who maintains the high-value crates (§7). Diff review on `cargo update` is currently informal; tightening this is tracked. |
| T-SC3 | A third-party GitHub Action used in CI gets a tag force-moved (or a branch hijacked) to point at malicious code, which then runs with `GITHUB_TOKEN` or signing-environment access. | H | ⚠️ | Most actions are pinned to a major/minor tag (`actions/checkout@v4`, `Swatinem/rust-cache@v2`, `rustsec/audit-check@v2.0.0`, `softprops/action-gh-release@v2`, etc.). `dtolnay/rust-toolchain@master` tracks a branch and is the weakest link. Signing/publish steps are gated on protected environments (T-B2), so a compromised check-job action cannot directly sign a release, but it could still exfiltrate source or tamper with the build that feeds the signing job. Pinning all actions to commit SHAs is tracked. |
| T-SC4 | Compromise of the upstream Rust toolchain (rustc / cargo) injects code into produced binaries. | M | ⚠️ | Toolchain version is pinned in CI (Rust 1.87). We rely on rust-lang's release signing and distribution; we do not independently verify toolchain hashes. Out of practical reach for this project; tracked rather than mitigated. |
| T-SC5 | A transitive crate is yanked from crates.io with no upstream replacement, so the audit job warns indefinitely and a freshly-resolved build cannot reproduce. | L | ⚠️ | `Cargo.lock` keeps existing builds compiling against the yanked version; new builds resolve the same locked version. CI surfaces the warning. Current example: `core2 0.3.3`. Policy: tracked per occurrence; bump the parent crate when an upstream fix lands. |
| T-SC6 | The published release binary cannot be independently verified to correspond to the source tree at the tagged commit — i.e. no reproducible builds and no SLSA provenance attestation. | M | ❌ | SHA256 checksums (T-B4) and platform code-signing (T-B2) prove the binary was produced by our release pipeline, but not that the pipeline built the source faithfully. A verifier with the source cannot today rebuild bit-for-bit. Tracked. |
| T-SC7 | A new direct dependency we add is a typosquat or dependency-confusion package masquerading as a legitimate crate. | M | ✅ | Project policy in `CLAUDE.md` requires explicit approval and an `~/.claude/approved-dependencies.md` entry before any new direct dependency is added, with package name, version, adoption signals, maintenance status, and license recorded. This relies on review discipline, not tooling, and is therefore a process control rather than a hard gate. |
| T-SC8 | `cargo update` silently pulls a malicious patch release within the semver range allowed by `Cargo.toml` between manual review windows. | M | ✅ | `Cargo.lock` is committed and version updates require a commit; CI runs against the lockfile. Auto-update bots (Dependabot/Renovate) are intentionally **not** configured, so dependency bumps are always human-driven and reviewable as a diff. |

The combined effect: we have strong reproducibility of *what we build today* (lockfile + pinned toolchain + pinned-by-tag actions), modest visibility into the integrity of *what those inputs are* (audit advisories only), and no independent verification of *what we publish* (no reproducible builds / provenance). Closing T-SC1, T-SC3, and T-SC6 is the priority. A practical next step is adopting `cargo-deny` (covers advisory + license + bans + sources in one tool, with a checked-in config) and SHA-pinning every action in `.github/workflows/`.

## 7. Dependency posture

Cargo dependencies are pinned via `Cargo.lock`. The high-value crates are the librustzcash family (`zcash_client_backend`, `zcash_client_sqlite`, `zcash_keys`, `zcash_protocol`, `zcash_primitives`, `zcash_transparent`, `sapling-crypto`, `orchard`), maintained by ZODL (formerly the ECC mobile team); `secrecy` and `secp256k1` for key handling; `rustls` (with the `ring` provider and no `aws-lc-sys`, per PR #54) for TLS; and Tauri for the GUI shell. CI runs `cargo audit` against the RustSec advisory database on every push and PR (T-B1); the documented advisory carve-outs live in `.cargo/audit.toml`.

JavaScript dependencies: **none at runtime**. The Tauri GUI ships zero npm packages in the browser bundle (PR #50).

For threats to the integrity of the dependency tree itself (build scripts, maintainer takeover, GitHub Actions tag-moving, reproducible builds, etc.), see §6.6.

## 8. Open issues and known gaps

These are intentionally listed in one place so the document drives a backlog rather than just describing the world:

- [x] **T-S2** — strip the seed from `state.scanConfig` in the GUI (PR #53 follow-up).
- [ ] **T-N2** — pin the certificate of `zec.rocks` / `na.zec.rocks` for the default endpoints.
- [ ] **T-N5** — surface the auto-detect privacy implication more loudly in the UI.
- [x] **T-L3** — add a "Delete workspace" action that securely wipes a session post-recovery.
- [x] **T-B1** — add `cargo audit` (or `cargo deny check advisories`) to CI on every push.
- [ ] **T-B3** — provision a Windows code-signing certificate and gate it behind the same protected-environment mechanism as macOS.
- [ ] **T-SC1** — adopt a tool that surfaces `build.rs` and proc-macro presence across the dependency tree (e.g. `cargo-deny` bans + `cargo geiger`), and consider sandboxing build scripts in CI (`CARGO_BUILD_RUSTFLAGS`/seccomp profiles).
- [ ] **T-SC2** — formalize a dependency-bump review checklist (diff the changelog, scan for new `build.rs` / network calls / proc macros) and record sign-off in the PR.
- [ ] **T-SC3** — pin all third-party GitHub Actions to commit SHAs (especially `dtolnay/rust-toolchain@master`), with a comment recording the resolved tag.
- [ ] **T-SC6** — investigate reproducible builds for release artifacts and publishing SLSA provenance attestations (e.g. via `slsa-github-generator`).

## 9. Out of scope

- Host OS compromise (root/admin malware).
- Side-channel attacks (cache, EM, power).
- Physical attacks on the user's machine (cold-boot, evil maid).
- Quantum-cryptographic attacks against Sapling/Orchard.
- User coercion / duress.
- Pre-Sapling (Sprout) note recovery — librustzcash dropped Sprout scanning long before this project began; ZWL seeds whose only funds are in Sprout notes (block <419,200, before October 2018) cannot be recovered via Argos.

## 10. Reporting a security issue

Please **do not** open a public GitHub issue for a security vulnerability. Email `security@sovright.com` with a description and reproduction steps; we will respond within five business days. No PGP key is available at this time. Plain email is sufficient for v0.1.0-rc.

## 11. Revision history

| Date | Author | Notes |
|---|---|---|
| 2026-05-19 | Zaki | Initial draft. Covers v0.1.0-rc. Open items listed in §8. |
| 2026-05-27 | Zaki | Added §6.6 Supply chain integrity (T-SC1..T-SC8) covering build scripts, maintainer takeover, third-party Actions, toolchain, yanked crates, reproducible builds, typosquatting, and `cargo update` discipline. Cross-referenced from §7 and §8. |
| 2026-05-13 | Kristi | Correct T-L1 status (permissions implemented); fix CSP quote; clarify T-N4 address count; PGP note. |
