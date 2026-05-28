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
| T-SC3 | A third-party GitHub Action used in CI gets a tag force-moved (or a branch hijacked) to point at malicious code, which then runs with `GITHUB_TOKEN` or signing-environment access. | H | ✅ | Every third-party Action is now SHA-pinned with a `# vX.Y.Z` trailing comment (see PR #70), and the repository-level **Actions → Require SHA pinning for third-party Actions** setting is enabled (T-SC10), so a workflow that regresses to a tag pin is refused at job-start time. Signing/publish steps remain gated on protected environments (T-B2). |
| T-SC4 | Compromise of the upstream Rust toolchain (rustc / cargo) injects code into produced binaries. | M | ⚠️ | Toolchain version is pinned in CI (Rust 1.87). We rely on rust-lang's release signing and distribution; we do not independently verify toolchain hashes. Out of practical reach for this project; tracked rather than mitigated. |
| T-SC5 | A transitive crate is yanked from crates.io with no upstream replacement, so the audit job warns indefinitely and a freshly-resolved build cannot reproduce. | L | ⚠️ | `Cargo.lock` keeps existing builds compiling against the yanked version; new builds resolve the same locked version. CI surfaces the warning. Current example: `core2 0.3.3`. Policy: tracked per occurrence; bump the parent crate when an upstream fix lands. |
| T-SC6 | The published release binary cannot be independently verified to correspond to the source tree at the tagged commit — i.e. no reproducible builds and no SLSA provenance attestation. | M | ❌ | SHA256 checksums (T-B4) and platform code-signing (T-B2) prove the binary was produced by our release pipeline, but not that the pipeline built the source faithfully. A verifier with the source cannot today rebuild bit-for-bit. Tracked. |
| T-SC7 | A new direct dependency we add is a typosquat or dependency-confusion package masquerading as a legitimate crate. | M | ✅ | Project policy in `CLAUDE.md` requires explicit approval and an `~/.claude/approved-dependencies.md` entry before any new direct dependency is added, with package name, version, adoption signals, maintenance status, and license recorded. This relies on review discipline, not tooling, and is therefore a process control rather than a hard gate. |
| T-SC8 | `cargo update` silently pulls a malicious patch release within the semver range allowed by `Cargo.toml` between manual review windows. | M | ✅ | `Cargo.lock` is committed and version updates require a commit; CI runs against the lockfile. Auto-update bots (Dependabot/Renovate) are intentionally **not** configured, so dependency bumps are always human-driven and reviewable as a diff. |
| T-SC9 | A pull request from a fork (external contributor) triggers our CI workflows, gaining `GITHUB_TOKEN` access and running attacker-controlled code on our runners — used either to exfiltrate via cache writes / artifacts / logs, or to consume CI minutes. | H | ✅ | Repository setting **Actions → Approval for running fork pull request workflows from contributors** set to `all_external_contributors` (verified via `gh api repos/.../actions/permissions/fork-pr-contributor-approval`). Every PR from a fork now requires explicit maintainer approval before any workflow runs. Combined with the protected `release-sign` / `release-publish` environments (T-B2), a fork PR cannot reach signing material even on approval. |
| T-SC10 | A future workflow change reintroduces a tag- or branch-pinned third-party Action (e.g. `actions/checkout@v4`), regressing the SHA-pin posture (T-SC3) silently between reviews. | M | ✅ | Repository setting **Actions → Require SHA pinning for third-party Actions** is enabled (verified via `gh api repos/.../actions/permissions` → `sha_pinning_required: true`). Workflow runs that reference a non-SHA-pinned third-party Action are refused by GitHub at job-start time, so a regression would surface as a CI failure rather than slipping in. |

The combined effect: we have strong reproducibility of *what we build today* (lockfile + pinned toolchain + pinned-by-tag actions), modest visibility into the integrity of *what those inputs are* (audit advisories only), and no independent verification of *what we publish* (no reproducible builds / provenance). Closing T-SC1, T-SC3, and T-SC6 is the priority. A practical next step is adopting `cargo-deny` (covers advisory + license + bans + sources in one tool, with a checked-in config) and SHA-pinning every action in `.github/workflows/`.

#### 6.6.1 Comparison with zebrad and Zodl

Argos shares the librustzcash dependency core with two other production Zcash projects, but with different distribution models and supply-chain postures. Honest comparison:

| Practice | Argos (this repo) | zebrad (ZcashFoundation/zebra) | Zodl iOS / Android (zodl-inc) |
|---|---|---|---|
| Rust dependency gate in CI | `cargo audit` (advisories only) | `cargo-deny` covering advisories, licenses, multiple-versions, wildcards, bans, and sources | n/a — Rust enters as a built artifact via the Zcash mobile SDKs, not compiled directly |
| Yanked-crate policy | Audit emits a warning; tracked per-occurrence (T-SC5). Currently `core2 0.3.3`. | `yanked = "deny"` in `deny.toml` — CI **fails** on any yanked dep | n/a |
| Third-party Action pinning | Most pinned to major-version tag (`@v4`, `@v2`); `dtolnay/rust-toolchain@master` floats on a branch (T-SC3) | `EmbarkStudios/cargo-deny-action` pinned to a 40-char commit SHA with the tag in a trailing comment | Mobile builds use Fastlane + platform CI; out of this comparison |
| Dependency-update bot | Intentionally **none**; bumps are human-driven and reviewable as a diff (T-SC8 ✅) | Dependabot present (`.github/dependabot.yml`) — auto-PRs are filed and gated through review + the `cargo-deny` job | Gradle lockfile on Android (`buildscript-gradle.lockfile`); SwiftPM on iOS |
| Lockfile commitment | `Cargo.lock` committed | `Cargo.lock` committed | `buildscript-gradle.lockfile` (Android); `Package.resolved` (iOS) |
| Release binary integrity | SHA256 checksums + macOS code-signing; Windows unsigned (T-B3); no provenance attestations (T-SC6) | Docker images on GitHub Packages + binary releases; relies on GitHub release artifact hosting + Docker pull verification | **App Store / Play Store** distribution — binary integrity is delegated to Apple/Google platform signing; users do not run unsigned binaries |
| Security disclosure | `security@sovright.com`, plain email (PGP intentionally not offered for v0.1.0-rc) | `security@zfnd.org` with a published PGP key; follows the RD-Crypto-Spec responsible-disclosure standard | `responsible_disclosure.md` published in repo with their process |

**What we should copy from zebrad.** The single highest-leverage change is **adopting `cargo-deny`** with a checked-in `deny.toml` that sets `yanked = "deny"`, restricts licenses to a known-good set, and bans wildcards / multiple-versions where feasible. zebrad's `deny.toml` is a good template. This subsumes our current `cargo audit` job (covered as `cargo-deny check advisories`) while also catching the yanked-crate case that today only produces a warning. SHA-pinning every third-party Action — as zebrad does for `cargo-deny-action` — is the second item.

**What does not transfer from Zodl.** Zodl mobile delegates binary-integrity to App Store / Play Store signing and review. Argos ships standalone binaries directly from GitHub Releases on three platforms, so we cannot offload that step the way Zodl can. The integrity guarantees in §6.5 (T-B2/T-B3/T-B4) and the provenance gap in T-SC6 exist because we are not in the mobile-store model. This is a structural difference, not a posture gap.

**Where we currently match or exceed.** No JavaScript runtime dependencies (§7) is a stronger position than either project: zebrad has no JS, Zodl has the full native-mobile dependency surface, and we sit between by deliberately not shipping an npm bundle. The conservative dependency-bump policy in `CLAUDE.md` is a process control zebrad does not document; we should keep it but recognize it is review discipline, not a hard gate (T-SC2).

**Where we are behind.** No `cargo-deny`, no SHA-pinned Actions, no SLSA provenance, no PGP disclosure key, and one Action (`dtolnay/rust-toolchain@master`) tracks a branch. Items addressable inside this repo are listed in §8 (T-SC1, T-SC3, T-SC6); PGP is a project-level v0.1.0-rc decision.

#### 6.6.2 librustzcash and the Zcash mobile Rust SDKs

The Zodl comparison in §6.6.1 stops at the mobile app, but Argos and Zodl share the *same* upstream Rust core — the librustzcash workspace at `zcash/librustzcash` (now ZODL-maintained per `MEMORY.md`). The Zcash mobile Rust SDKs at `zcash/zcash-android-wallet-sdk` (Kotlin) and `zcash/zcash-swift-wallet-sdk` (Swift) **embed librustzcash as an in-tree Rust submodule** and build it into the platform binding (`backend-lib/` on Android, a `rust/` directory + `Cargo.lock` + `Package.resolved` on iOS). Argos consumes the same crates from crates.io. The dependency graph that flows into a built Argos binary, a built Zodl iOS binary, and a built Zodl Android binary therefore shares its largest single chunk.

The posture of that shared upstream is markedly stronger than ours, zebrad's, or the mobile apps' own platform layer:

| Practice | Argos (today) | zebrad | librustzcash | Zcash mobile SDKs |
|---|---|---|---|---|
| Per-crate code audit (third-party crate review, not just advisories) | **none** | none (advisories only via `cargo-deny`) | **`cargo-vet`** with imports from Bytecode Alliance, Embark, Fermyon, Google, ISRG (libprio), Mozilla, and Zcash's own audit set; custom criteria including `crypto-reviewed` and `license-reviewed`; named human auditors per delta (`who = "Kris Nuttycombe <kris@nutty.land>"`, `Daira-Emma Hopwood`, etc.) | Inherit librustzcash's audits transitively because they embed the same workspace |
| License gate | none (we plan `cargo-deny`) | `cargo-deny check licenses` (broad allow-list) | `cargo-deny check licenses` with `allow = ["Apache-2.0", "MIT"]` only — every other SPDX is named per-crate as an exception. Strictest in the comparison. | Inherit librustzcash's `deny.toml` for the embedded Rust workspace |
| Multi-target dependency graph vetting | n/a (one target per platform) | single target | `[graph] targets` enumerates 14 triples (Linux/macOS/Windows/iOS/Android/FreeBSD) so the dep tree is vetted under every consumer's build configuration | Inherited |
| GitHub Actions security analysis | none | none | **`zizmor`** (`zizmor-action`) on every push and PR, with `permissions: {}` at the workflow top level and `persist-credentials: false` on checkout — least-privilege workflows | The Swift SDK also runs `zizmor` + `codeql` on its workflows |
| Action pin form | most tag-pinned, one (`dtolnay/rust-toolchain@master`) on a branch | tag-pinned with one SHA pin (`cargo-deny-action`) | **SHA-pinned with version comment** for every third-party Action (e.g. `actions/checkout@de0fac2…f5447ce83dd # v6.0.2`, `EmbarkStudios/cargo-deny-action@6c8f9fa…b7b7777d1 # v2.0.18`) — the practice T-SC3 calls for | Same SHA-pin pattern |
| Mutation / quality posture adjacent to supply chain | none | none | **`cargo-mutants`** in CI (`mutants.yml`) — separate goal but raises the bar for any malicious code change going undetected | Inherits the Rust core; platform-side test suites separate |
| Disclosure | plain `security@sovright.com`, no PGP for v0.1.0-rc | PGP-keyed (`zfnd.org`), follows RD-Crypto-Spec | inherits ECC / ZODL process | PGP-keyed (`security@z.cash`), follows RD-Crypto-Spec |

The honest takeaway: the upstream Rust supply chain we consume is auditing itself more rigorously than we audit our consumption of it.

#### 6.6.3 What Argos should adopt from each

Concrete, in priority order. These map onto the §8 backlog entries and replace the earlier "next step is cargo-deny" framing with something more grounded now that the comparison set is complete.

**From librustzcash — the highest-leverage items.**

1. **`cargo-vet` with imports from librustzcash's audit set.** Adopt `cargo-vet` and a `supply-chain/config.toml` that lists `[imports.zcash] url = "https://raw.githubusercontent.com/zcash/librustzcash/main/supply-chain/audits.toml"`, plus the same federated imports librustzcash already uses (Bytecode Alliance, Embark, Fermyon, Google, ISRG, Mozilla). This inherits a large vetted-crates database for free — the crates we share with librustzcash (essentially everything under the librustzcash umbrella) are covered the moment we add the import line. First-party audits are then only required for crates we add that librustzcash + their import set do not already vet (Tauri being the obvious unaudited surface). Closes T-SC1 in a substantive way; closes T-SC2 partially by introducing named-auditor accountability for any new direct dependency.

2. **`zizmor` on `.github/workflows/`.** Add a workflow that runs `zizmorcore/zizmor-action` against our own workflows on every push and PR. Cost: one workflow file, one cargo-equivalent install. Catches the kinds of misconfigurations T-SC3 is about (overly permissive `permissions:` blocks, persistent credentials, command injection via untrusted GitHub event payloads). Already in use by librustzcash and the Swift SDK.

3. **SHA-pin every third-party Action with a `# vX.Y.Z` trailing comment.** Replace `dtolnay/rust-toolchain@master` (currently a floating branch) and the `@v4` / `@v2` tags on `actions/checkout`, `Swatinem/rust-cache`, `actions/setup-node`, `actions/upload-artifact`, `actions/download-artifact`, `softprops/action-gh-release`, `rustsec/audit-check` with 40-character commit SHAs and the resolved version in a comment. Matches librustzcash's pattern exactly. Closes T-SC3.

4. **Least-privilege workflows.** Add a top-level `permissions: {}` to each workflow and grant scopes per-job only as needed (already present on the `audit` job; not yet on the others). Add `persist-credentials: false` to every `actions/checkout` step. Both are mechanical changes librustzcash applies uniformly.

**From zebrad — the practical foundation.**

5. **`cargo-deny` with a `deny.toml`.** Adopt as the immediate-term consolidation of `cargo audit` plus license + bans + sources checking, modelled on zebrad's `deny.toml`. (`librustzcash` does this too, but its allow-list is stricter than we can run today without adding many exceptions; zebrad's mid-strictness allow-list is a closer fit for v0.1.0-rc.) Set `multiple-versions = "warn"` initially because the Tauri + librustzcash stack has known semver duplicates we should chip at gradually, not block on. Sequencing-wise this is the first thing to land — it sets up the surface that cargo-vet then deepens.

6. **`yanked = "warn"` then `"deny"` once `core2 0.3.3` resolves upstream.** Zebrad runs `yanked = "deny"` as a hard gate. We cannot land that today because of the currently-yanked `core2`, but the goal is identical and the flip is one line in `deny.toml` when upstream catches up.

**From the Zcash mobile SDKs — limited but real.**

7. **PGP-keyed responsible disclosure.** Both mobile SDKs and zebrad publish a PGP key and follow the RD-Crypto-Spec coordinated-disclosure standard. Argos explicitly dropped PGP for v0.1.0-rc (commit `3406c83`); this is a v0.1.0+1 decision and worth revisiting once we have a security mailing address with a steward.

8. **What does not transfer.** Mobile delegation of binary integrity to App Store / Play Store signing has no equivalent in our standalone-binary distribution model. SwiftPM's `Package.resolved` and Android's `buildscript-gradle.lockfile` correspond to our `Cargo.lock` and are already in place.

**Sequencing.** A reasonable order of adoption that an afternoon-per-item engineer can execute: (5) `cargo-deny` → (3) SHA-pin Actions → (2) `zizmor` → (4) least-privilege workflows → (1) `cargo-vet` with librustzcash imports → (6) flip yanked to deny when upstream resolves. (7) is a separate project-level decision; (8) is structural and out of scope.

#### 6.6.4 How much of our dependency surface actually diverges?

The §6.6.1–6.6.3 comparisons argue that we should adopt upstream practices; this subsection quantifies the *opportunity*, because the value of inheriting an upstream's audit posture depends on how much of our tree they actually cover.

Comparing `Cargo.lock` crate sets (verified 2026-05-27 against `ZcashFoundation/zebra` and `zcash/librustzcash` `main`):

| Set | Crates |
|---|---|
| Argos total | 618 |
| Shared with librustzcash | 364 (59%) |
| Shared with zebra | 423 (68%) |
| Shared with **either** upstream | **452 (73%)** |
| Unique to Argos (in neither) | **166 (27%)** |

So **73% of Argos's dependency surface is already exercised — and in librustzcash's case, audited — by an upstream Zcash project**. Adopting `cargo-vet` with `[imports.zcash]` (T-SC1, §6.6.3) captures that majority for free.

The 27% that diverges is overwhelmingly **the Tauri desktop-GUI stack**, broken down roughly as:

- **~52+ crates** in the Tauri / WebView / GTK3 stack: `tauri`, `wry`, the full `gtk-rs` family (`gtk`/`gdk`/`atk`/`gio`/`glib`/`gobject-sys`/`gtk3-macros`/`gdkwayland-sys`/`gdkx11`), `cairo-rs` + `cairo-sys-rs`, `webkit2gtk-*`, `javascriptcore-rs`, `core-graphics`, `cocoa`/`objc` (macOS), `libappindicator`, `embed_plist` (macOS bundling), `embed-resource` (Windows), `kuchikiki` + `html5ever` + `cssparser` + `selectors` (HTML/CSS Tauri uses internally), `keyboard-types`, `dpi`, `cookie`, `ico`, `infer`, `json-patch`.
- **~15 crates** in the `smol`/`async-std` runtime adjacent to Tauri's internal IPC — `async-broadcast`, `async-channel`, `async-executor`, `async-io`, `async-lock`, `async-process`, `async-signal`, `async-task`, `blocking`, `event-listener-strategy`, `futures-lite`, etc. — pulled by Tauri even though our application code uses `tokio`.
- **A handful of CLI helpers** unique to us: `dialoguer`, `keepawake`.
- **The remainder** (~95 crates): compression (`brotli`, `brotli-decompressor`, `fdeflate`), HTML/text helpers (`dom_query`, `futf`, `cesu8`), build-tooling adjacent (`cargo_toml`, `cfg-expr`, `cfb`, `ctor`, `ico`, `infer`), Unicode/i18n (`icu_locale_core`), and miscellaneous utility crates.

This overlaps almost perfectly with the unmaintained-crate ignores already documented in `.cargo/audit.toml` (RUSTSEC-2024-0411..0420, 2024-0429, 2025-0080, 2025-0081, 2025-0100) — those are exactly the gtk-rs GTK3 transitive set Tauri pulls in.

**Practical implications:**

1. The 73% we share is well-trodden ground. Bumps to that part of the tree carry low novel risk because both upstreams exercise it and librustzcash audits it.
2. The 27% that's ours alone is where new supply-chain risk concentrates. Future first-party `cargo-vet` audits should focus here first; importing more federated audit sets (Mozilla / Google / Embark already in §6.6.3's plan) covers some of the async-runtime tail but is light on the gtk-rs family.
3. Shrinking the divergence meaningfully requires one of: (a) Tauri upstream migrating to GTK4 (out of our hands; an upstream-scale change), (b) finding an audit source that targets the desktop-GUI stack specifically (none in our current import set does), or (c) first-party audits of the Tauri tree, which is real effort.

The honest framing: T-SC1's `cargo-vet` adoption gives us large coverage cheaply; the remaining work to drive exemptions toward zero is concentrated in a single, structurally hard-to-audit subsystem.

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
- [ ] **T-SC1** — adopt `cargo-deny` as the immediate consolidation (advisories + licenses + bans + sources), then `cargo-vet` with imports from librustzcash's audit set + the federated databases it already pulls (Bytecode Alliance, Embark, Fermyon, Google, ISRG, Mozilla). Sequence detailed in §6.6.3.
- [ ] **T-SC1b** — adopt `zizmor` (`zizmorcore/zizmor-action`) on `.github/workflows/` to catch overly permissive token usage, credential persistence, and command-injection patterns in our own workflows. Matches librustzcash and the Zcash Swift SDK.
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
| 2026-05-27 | Zaki | Added §6.6.1 comparing supply-chain posture to zebrad (ZcashFoundation/zebra) and Zodl (zodl-inc/zodl-{ios,android}). Highest-leverage gap identified: adopt `cargo-deny` with `yanked = "deny"` and SHA-pin all GitHub Actions (zebrad's pattern). |
| 2026-05-27 | Zaki | Added §6.6.2 extending the comparison to librustzcash and the Zcash mobile Rust SDKs (zcash-android-wallet-sdk, zcash-swift-wallet-sdk). Documented librustzcash's `cargo-vet` posture with federated audit imports from Bytecode Alliance / Embark / Fermyon / Google / ISRG / Mozilla, `zizmor` on workflows, and uniformly SHA-pinned Actions. Added §6.6.3 listing what Argos should adopt from each project, in sequence: `cargo-deny` → SHA-pin Actions → `zizmor` → least-privilege workflows → `cargo-vet` with `[imports.zcash]` → flip `yanked` to deny. T-SC1 split into T-SC1 (cargo-deny + cargo-vet) and T-SC1b (zizmor). |
| 2026-05-27 | Zaki | Added §6.6.4 quantifying dependency-surface divergence: of Argos's 618 transitive crates, 452 (73%) are shared with zebra or librustzcash and 166 (27%) are unique to Argos. The unique surface is essentially the Tauri desktop-GUI stack (~52 gtk-rs / WebKit / Cairo / WebView crates, ~15 smol-runtime crates, plus CLI helpers and misc utilities) — overlapping the unmaintained advisory ignores in `.cargo/audit.toml`. Implication: `cargo-vet` with `[imports.zcash]` captures the majority cheaply; shrinking the rest requires either Tauri upstream migration or first-party audits of the GUI tree. |
| 2026-05-28 | Zaki | Added T-SC9 (fork-PR CI execution by external contributors) and T-SC10 (regression of the SHA-pin posture). Both ✅ via repository-level Actions settings: `fork-pr-contributor-approval` set to `all_external_contributors` so every external PR requires maintainer approval before workflows run, and `sha_pinning_required: true` so any future workflow referencing a non-SHA-pinned third-party Action is refused at job-start time. T-SC3 status upgraded from ⚠️ to ✅ on the back of T-SC10. |
| 2026-05-13 | Kristi | Correct T-L1 status (permissions implemented); fix CSP quote; clarify T-N4 address count; PGP note. |
