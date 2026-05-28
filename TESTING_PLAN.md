# ZECK Testing Plan -- Week of 2026-04-13

**Participants:** Zaki, poldsam, + Claude instances  
**Goal:** Comprehensive test coverage before release. Each section has an owner and can be worked in parallel.

---

## Current State

- 13 unit tests across scan.rs, lightwalletd.rs, service.rs
- CI only runs `cargo check` (no `cargo test`, no clippy)
- No integration tests against lightwalletd
- No CLI end-to-end tests
- No GUI tests
- Sweep execution may not be fully wired (proposal-only in current state)

---

## Track 1: Unit Test Expansion (Claude-suitable)

**Owner:** Claude instances  
**Estimated effort:** 1-2 days  
**Approach:** TDD -- add tests, verify they fail or pass as expected

### 1A. derivation.rs

- [ ] Valid 24-word mnemonic produces deterministic seed bytes
- [ ] Invalid mnemonic (bad checksum) returns `InvalidMnemonic` error
- [ ] 12-word mnemonic rejected (ZWL used 24-word only)
- [ ] Derived Sapling extended spending key matches known test vector (if available from ZWL docs)
- [ ] Derived transparent address matches known test vector
- [ ] Account index 0 vs index 1 produce different keys
- [ ] Mainnet vs testnet produce different addresses (coin type 133 vs 1)

### 1B. address.rs

- [ ] Valid Unified Address with Orchard+Sapling receivers accepted
- [ ] Sapling-only address accepted
- [ ] Transparent-only address rejected (no shielded receiver)
- [ ] Mainnet address rejected when network=testnet
- [ ] Empty string rejected
- [ ] Malformed address rejected

### 1C. birthday.rs

- [ ] Sapling activation date (2018-10-28) returns height ~419200
- [ ] Date before Sapling activation returns Sapling activation height
- [ ] Future date returns reasonable estimate
- [ ] Known date/height pair validates correctly

### 1D. error.rs

- [ ] All error variants display meaningful messages
- [ ] Error conversion from anyhow preserves context

### 1E. models.rs

- [ ] ScanPhase transitions are valid
- [ ] AccountBalancePreview formatting
- [ ] SweepProposal serialization round-trip

---

## Track 2: Integration Tests -- lightwalletd (Zaki or poldsam)

**Owner:** Zaki  
**Estimated effort:** 2-3 days  
**Requires:** Network access to lightwalletd endpoints  
**Note:** These tests hit real infrastructure -- keep them behind `#[ignore]` or a feature flag (`integration-tests`) so CI doesn't depend on external services

### 2A. Connection & Probing

- [ ] Connect to mainnet default endpoint, verify `GetLightdInfo` returns valid chain info
- [ ] Connect to testnet endpoint, verify different chain params
- [ ] Invalid endpoint URL returns meaningful error
- [ ] Endpoint fallback: first endpoint down, second works -- verify recovery
- [ ] TLS verification works (reject self-signed certs)

### 2B. Block Streaming

- [ ] Stream a small range of blocks (e.g., 10 blocks from a known height)
- [ ] Verify block data is non-empty and parseable
- [ ] Verify block heights are sequential and match requested range

### 2C. Scanning a Known Wallet

- [ ] **Create a testnet wallet with known seed, send funds to it via faucet**
- [ ] Scan testnet with known seed -- verify expected balance discovered
- [ ] Scan mainnet with empty seed (fresh random) -- verify zero balance, gap-limit triggers
- [ ] Verify workspace persistence: scan, stop, resume -- same results

---

## Track 3: CLI End-to-End Tests (Claude-suitable)

**Owner:** Claude instances  
**Estimated effort:** 1-2 days  
**Approach:** Use `assert_cmd` or similar crate for CLI testing

### 3A. show-keys Subcommand

- [ ] Valid seed outputs derived keys and addresses
- [ ] `--num-accounts 3` shows exactly 3 accounts
- [ ] `--network testnet` changes coin type in derivation paths
- [ ] Invalid seed shows error, exits non-zero
- [ ] `--seed-file` reads from file

### 3B. scan Subcommand (mocked or testnet)

- [ ] Missing `--seed` or `--seed-file` prompts interactively or errors
- [ ] `--birthday-date 2020-01-01` converts to block height
- [ ] `--gap-limit 0` rejected
- [ ] `--gap-limit 51` rejected (max 50)
- [ ] `--num-accounts 501` rejected (max 500)
- [ ] `--verbose` enables debug output

### 3C. sweep Subcommand (dry-run only)

- [ ] Missing `--destination` returns error
- [ ] Invalid destination address returns clear error
- [ ] Without `--confirm-sweep`, outputs proposal but does not broadcast
- [ ] `--max-fee` below estimated fee rejects sweep

---

## Track 4: Sweep Pipeline Testing (poldsam)

**Owner:** poldsam  
**Estimated effort:** 2-3 days  
**Requires:** Testnet ZEC, testnet lightwalletd access

### 4A. Sweep Proposal Logic

- [ ] Multi-account wallet produces correct number of proposed transactions
- [ ] Transparent-only account generates shield tx first, then sweep
- [ ] Sapling-only account generates single sweep tx
- [ ] Mixed account generates shield + sweep
- [ ] Dust accounts (below fee) excluded from proposal
- [ ] Total fees across all txs match ZIP 317 expectations

### 4B. Testnet Sweep Execution

- [ ] **Setup:** Create testnet wallet, fund with testnet ZEC across multiple pools
  - Account 0: Sapling balance
  - Account 1: Transparent balance
  - Account 2: Orchard balance
  - Account 3: Empty (verify skipped)
- [ ] Scan discovers all funded accounts
- [ ] Propose sweep shows correct summary
- [ ] Execute sweep with `--confirm-sweep` broadcasts transactions
- [ ] Verify transactions appear on testnet block explorer
- [ ] Verify destination address receives expected net amount (total - fees)
- [ ] Verify recovery report contains all tx IDs and statuses

### 4C. Edge Cases

- [ ] Sweep with zero sweepable balance (all dust) -- graceful message
- [ ] Sweep after partial previous sweep -- only remaining funds proposed
- [ ] Network disconnect during sweep -- partial broadcast handling
- [ ] Max-fee guard prevents unexpectedly expensive sweeps

---

## Track 5: GUI Testing (Zaki + poldsam)

**Owner:** Zaki + poldsam  
**Estimated effort:** 1-2 days  
**Approach:** Manual walkthrough on macOS (both Apple Silicon and Intel if available)

### 5A. Screen Flow

- [ ] Welcome screen renders, "I have my seed phrase" button works
- [ ] Seed entry: paste 24 words, validate button shows success
- [ ] Seed entry: paste invalid words, shows clear error
- [ ] Seed entry: show/hide toggle works
- [ ] Configure screen: all fields pre-populated with defaults
- [ ] Configure: change birthday date, verify height updates
- [ ] Configure: enter custom lightwalletd URL
- [ ] Configure: enter destination UA, validates on blur/submit

### 5B. Scan Screen

- [ ] Progress bar updates during scan
- [ ] Account table populates as accounts are discovered
- [ ] ETA updates reasonably
- [ ] Cancel button stops scan
- [ ] Resume after cancel continues from persisted state

### 5C. Sweep Screen

- [ ] Summary matches CLI proposal output
- [ ] Irreversibility warning displays
- [ ] Checkbox required before sweep button activates
- [ ] Sweep button broadcasts (testnet only for testing)

### 5D. Completion Screen

- [ ] Transaction IDs displayed
- [ ] Status updates (broadcast -> confirmed)
- [ ] "Save Recovery Report" generates readable text file
- [ ] Report contains: seed fingerprint (NOT seed), accounts, tx IDs, amounts, destination

---

## Track 6: Security Review (Claude instance)

**Owner:** Claude instance  
**Estimated effort:** 0.5 day

- [ ] Seed phrase never appears in log output (grep all tracing calls)
- [ ] Seed phrase never passed via Tauri IPC as plain text in event payloads
- [ ] `SecretString` used consistently -- no `String` copies of seed material
- [ ] Workspace directory permissions are user-only (0700)
- [ ] Recovery report does NOT contain seed phrase
- [ ] Tauri capabilities restrict IPC to only needed commands
- [ ] No hardcoded secrets or API keys in source

---

## Track 7: CI Hardening (Claude-suitable)

**Owner:** Claude instance  
**Estimated effort:** 0.5 day

- [ ] Add `cargo test -p zeck-core` to CI
- [ ] Add `cargo clippy --workspace -- -D warnings` to CI
- [ ] Add `cargo fmt --check` to CI
- [ ] Consider: matrix build for ubuntu + macOS
- [ ] Consider: GUI build step (`cd gui && npm ci && cargo tauri build`)

---

## Scheduling Suggestion

| Day | Zaki | poldsam | Claude Instances |
|-----|------|---------|-----------------|
| Mon | Track 2A-2B setup | Track 4A sweep logic review | Track 1 (all unit tests) |
| Tue | Track 2C known wallet | Track 4B testnet setup + funding | Track 3 (CLI e2e tests) |
| Wed | Track 5A-5B GUI manual | Track 4B sweep execution | Track 6 (security review) |
| Thu | Track 5C-5D GUI sweep/complete | Track 4C edge cases | Track 7 (CI hardening) |
| Fri | Review all results, triage failures | Review all results, triage failures | Fix any test failures from triage |

---

## Test Infrastructure Needed

1. **Testnet seed phrase** -- generate a dedicated test wallet, record the 24 words somewhere secure (NOT in git)
2. **Testnet ZEC** -- fund the test wallet from faucet across Sapling, Orchard, and transparent pools
3. **`assert_cmd` crate** -- for CLI integration tests (needs user approval per dependency policy)
4. **Feature flag `integration-tests`** -- gate network-dependent tests so they don't run in normal CI

---

## Success Criteria

- All Track 1 + Track 3 tests pass in CI
- Track 2 integration tests pass manually with documented lightwalletd endpoints
- Track 4 demonstrates end-to-end sweep on testnet with verified destination receipt
- Track 5 GUI walkthrough completed on at least one platform with no blocking issues
- Track 6 finds no seed leakage paths
- Track 7 CI runs tests + clippy + fmt on every PR
