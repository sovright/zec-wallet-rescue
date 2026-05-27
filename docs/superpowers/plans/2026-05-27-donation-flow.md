# Donation Flow Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a user direct a configurable share (default 10%) of recovered funds to the project as an extra shielded output on each sweep transaction, with an optional email in the donation memo for off-chain receipts.

**Architecture:** A baked-in shielded `DONATION_ADDRESS` constant in `zeck-core` drives the feature; an empty constant disables it everywhere. A pure `donation` module owns the split math and memo formatting, reused by both the dry-run proposal (`build_sweep_proposal`) and the real broadcast path (`execute_send_max_step`). The broadcast path uses a two-pass build: a send-max dry run to measure the spendable amount and fee, then a fixed two-payment `propose_transfer` that absorbs the extra output's marginal fee into the user's remainder. The GUI adds an editable donation form on the Review & Sweep screen and repoints the existing PR #66 overlay/card at the baked address.

**Tech Stack:** Rust (`zcash_client_backend` proposal machinery, ZIP-317 fees), Tauri v2 commands, static HTML/JS frontend (`withGlobalTauri`).

**Spec:** `docs/superpowers/specs/2026-05-27-donation-flow-design.md`

**Branch:** Build on `feat/donate-flow` (PR #66). Bring this plan + the spec onto that branch first (cherry-pick the spec/plan commits or re-commit) so the work travels with the branch.

**Pre-flight (do before Task 1):**
- [ ] `git checkout feat/donate-flow` (or rebase this branch onto it). Tasks 7–9 assume PR #66's overlay markup and `initDonate`/`DONATE_ZEC_ADDRESS`/`donate-address` elements are present — they exist only on `feat/donate-flow`, not on `main`/`chore/bump-zcash-deps`.
- [ ] `cargo build -p argos-core` and `cargo test -p argos-core` — confirm a clean baseline.

---

## File Structure

**Create:**
- `crates/zeck-core/src/donation.rs` — baked constants + pure helpers (split math, memo formatting, address parsing, feature-enabled check). One responsibility: everything donation-specific that does not touch the wallet DB.

**Modify:**
- `crates/zeck-core/src/lib.rs` — declare `mod donation;` and re-export the public items used by `service.rs` / commands.
- `crates/zeck-core/src/models.rs` — add fields to `SweepRequest`, `ProposedTx`, `SweepProposal`.
- `crates/zeck-core/src/service.rs` — `propose_sweep` passes network; `build_sweep_proposal` splits the shielded sweep estimate; `execute_send_max_step` does the two-pass donation build; `propose_sweep`/`execute_sweep` thread the new request fields.
- `gui/src-tauri/src/commands.rs` — `propose_sweep`/`execute_sweep` commands accept `donation_rate` + `donor_email`; new `donation_address` command.
- `crates/zeck-cli/src/main.rs` — `--donation-rate` / `--donor-email` flags threaded into `SweepRequest`.
- `gui/src/index.html` — donation form on the Review & Sweep screen; overlay address element id unchanged.
- `gui/src/styles.css` — styles for the new form (reuse existing classes where possible).
- `gui/src/main.js` — wire the form, compute live amounts from `proposal.total_donation_zatoshis`, hide on testnet, populate overlay from the `donation_address` command.

**Shared-math rule (DRY):** the donation amount for a given send amount is computed by exactly one function, `donation::donation_for_send_amount`, called from both `build_sweep_proposal` and `execute_send_max_step`. Do not duplicate the formula.

---

## Task 1: Donation module — constants and pure helpers (TDD)

**Files:**
- Create: `crates/zeck-core/src/donation.rs`
- Modify: `crates/zeck-core/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/zeck-core/src/donation.rs` with only the test module first:

```rust
//! Donation feature: baked-in recipient and pure split/memo helpers.
//!
//! An empty `DONATION_ADDRESS` disables the feature everywhere — donation
//! outputs are never created and the GUI hides its donation affordances.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_address_empty() {
        // With no baked address, the feature is off regardless of inputs.
        assert!(!feature_enabled(""));
        assert_eq!(donation_for_send_amount("", Some(0.10), 1_000_000), 0);
    }

    #[test]
    fn no_donation_when_rate_none_or_zero() {
        assert_eq!(donation_for_send_amount(SOME_ADDR, None, 1_000_000), 0);
        assert_eq!(donation_for_send_amount(SOME_ADDR, Some(0.0), 1_000_000), 0);
    }

    #[test]
    fn donation_is_rate_times_send_amount_when_above_threshold() {
        // 10% of 2_000_000 = 200_000 >= MIN_DONATION_ZATOSHIS (100_000)
        assert_eq!(donation_for_send_amount(SOME_ADDR, Some(0.10), 2_000_000), 200_000);
    }

    #[test]
    fn donation_suppressed_below_threshold() {
        // 10% of 500_000 = 50_000 < 100_000 → no donation
        assert_eq!(donation_for_send_amount(SOME_ADDR, Some(0.10), 500_000), 0);
    }

    #[test]
    fn donation_never_exceeds_send_amount() {
        // Defensive: a rate >= 1.0 is clamped so the remainder stays positive.
        let d = donation_for_send_amount(SOME_ADDR, Some(1.0), 2_000_000);
        assert!(d < 2_000_000);
    }

    #[test]
    fn memo_is_tag_only_without_email() {
        assert_eq!(donation_memo_body(None), DONATION_MEMO_TAG.to_owned());
    }

    #[test]
    fn memo_appends_email_line_when_present() {
        let body = donation_memo_body(Some("a@b.com"));
        assert_eq!(body, format!("{DONATION_MEMO_TAG}\na@b.com"));
    }

    #[test]
    fn memo_omits_blank_email() {
        assert_eq!(donation_memo_body(Some("   ")), DONATION_MEMO_TAG.to_owned());
    }

    #[test]
    fn rate_validation_rejects_out_of_range() {
        assert!(validate_donation_rate(Some(1.5)).is_err());
        assert!(validate_donation_rate(Some(-0.1)).is_err());
        assert!(validate_donation_rate(Some(0.10)).is_ok());
        assert!(validate_donation_rate(None).is_ok());
    }

    #[test]
    fn email_validation_is_lenient_but_requires_at() {
        assert!(validate_donor_email(None).is_ok());
        assert!(validate_donor_email(Some("")).is_ok()); // treated as absent
        assert!(validate_donor_email(Some("a@b.com")).is_ok());
        assert!(validate_donor_email(Some("notanemail")).is_err());
    }

    // A syntactically valid mainnet UA for tests (does not need to be the real
    // donation address). Use any UA already present in existing test fixtures;
    // if none, derive one from the CLAUDE.md test seed in a throwaway script.
    const SOME_ADDR: &str = "u1..."; // TODO replace with a valid test UA
}
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cargo test -p argos-core donation:: 2>&1 | head -30`
Expected: compile errors — `feature_enabled`, `donation_for_send_amount`, etc. not found.

- [ ] **Step 3: Implement the module**

Add above the test module in `crates/zeck-core/src/donation.rs`:

```rust
use crate::error::{ZeckError, ZeckResult};

/// Baked-in donation recipient. MUST be a shielded Unified Address (memos
/// require a shielded output). Empty string disables the donation feature
/// everywhere. Set this to the real address to activate.
pub const DONATION_ADDRESS: &str = "";

/// Fixed label so all sweep-sourced donations are identifiable when the
/// project scans the donation address's memos.
pub const DONATION_MEMO_TAG: &str = "Argos sweep donation v1";

/// Suggested default donation share, shown pre-filled in the GUI.
pub const DEFAULT_DONATION_RATE: f64 = 0.10;

/// Below this, no donation output is created for a transaction (0.001 ZEC).
/// Comfortably above the marginal ZIP-317 cost of one extra output.
pub const MIN_DONATION_ZATOSHIS: u64 = 100_000;

/// Whether the donation feature is active for a given baked address.
pub fn feature_enabled(address: &str) -> bool {
    !address.trim().is_empty()
}

/// Donation amount (zatoshis) for one account's send amount.
///
/// Returns 0 (no donation output) when the feature is disabled, the rate is
/// absent/zero, or the computed donation is below `MIN_DONATION_ZATOSHIS`.
/// Clamped to strictly less than `send_amount` so the user's remainder stays
/// positive. Callers are responsible for skipping the feature on testnet.
pub fn donation_for_send_amount(address: &str, rate: Option<f64>, send_amount: u64) -> u64 {
    if !feature_enabled(address) {
        return 0;
    }
    let rate = match rate {
        Some(r) if r > 0.0 => r.min(0.99),
        _ => return 0,
    };
    let donation = (send_amount as f64 * rate).round() as u64;
    if donation < MIN_DONATION_ZATOSHIS || donation >= send_amount {
        return 0;
    }
    donation
}

/// Memo body for the donation output: tag alone, or tag + email line.
pub fn donation_memo_body(email: Option<&str>) -> String {
    match email.map(str::trim).filter(|e| !e.is_empty()) {
        Some(email) => format!("{DONATION_MEMO_TAG}\n{email}"),
        None => DONATION_MEMO_TAG.to_owned(),
    }
}

/// Validate the requested donation rate. `None` is valid (skip).
pub fn validate_donation_rate(rate: Option<f64>) -> ZeckResult<()> {
    match rate {
        None => Ok(()),
        Some(r) if (0.0..=1.0).contains(&r) => Ok(()),
        Some(r) => Err(ZeckError::InvalidConfig(format!(
            "donation rate {r} must be between 0.0 and 1.0"
        ))),
    }
}

/// Lenient email validation. Empty/None is valid (no receipt requested).
pub fn validate_donor_email(email: Option<&str>) -> ZeckResult<()> {
    match email.map(str::trim).filter(|e| !e.is_empty()) {
        None => Ok(()),
        Some(e) if e.contains('@') && !e.starts_with('@') && !e.ends_with('@') => Ok(()),
        Some(e) => Err(ZeckError::InvalidConfig(format!("invalid email: {e}"))),
    }
}
```

In `crates/zeck-core/src/lib.rs`, add the module declaration alongside the other `mod` lines and re-export:

```rust
mod donation;
pub use donation::{
    donation_for_send_amount, donation_memo_body, feature_enabled as donation_enabled,
    validate_donor_email, validate_donation_rate, DEFAULT_DONATION_RATE, DONATION_ADDRESS,
    DONATION_MEMO_TAG, MIN_DONATION_ZATOSHIS,
};
```

Replace the `SOME_ADDR` TODO with a real valid test UA (see note in the test).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p argos-core donation:: 2>&1 | tail -20`
Expected: all donation tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/zeck-core/src/donation.rs crates/zeck-core/src/lib.rs
git commit -m "feat(core): add donation module with split math and memo helpers"
```

---

## Task 2: Extend sweep models

**Files:**
- Modify: `crates/zeck-core/src/models.rs:289-335`
- Touches (to keep compiling): `service.rs`, `commands.rs`, `cli/main.rs` construction sites

- [ ] **Step 1: Add fields**

In `SweepRequest` (after `max_fee_zatoshis`):

```rust
    /// Fraction of each account's send amount to donate (e.g. 0.10). `None`
    /// skips the donation entirely. Validated to 0.0..=1.0.
    #[serde(default)]
    pub donation_rate: Option<f64>,
    /// Optional email placed in the donation memo for an off-chain receipt.
    #[serde(default)]
    pub donor_email: Option<String>,
```

In `ProposedTx` (after `net_zatoshis`):

```rust
    /// Portion of `net_zatoshis` routed to the donation address (0 if none).
    #[serde(default)]
    pub donation_zatoshis: u64,
```

In `SweepProposal` (after `net_received_zatoshis`):

```rust
    /// Sum of `donation_zatoshis` across all transactions.
    #[serde(default)]
    pub total_donation_zatoshis: u64,
```

- [ ] **Step 2: Update construction sites to compile (set new fields to defaults for now)**

- `service.rs` `build_sweep_proposal`: add `donation_zatoshis: 0,` to both `ProposedTx { .. }` literals and `total_donation_zatoshis: 0,` to the `SweepProposal { .. }` literal. (Task 3 fills these in.)
- `gui/src-tauri/src/commands.rs:160` and `:190`: add `donation_rate: None,` and `donor_email: None,` to both `SweepRequest { .. }` literals. (Task 5 fills these in.)
- `crates/zeck-cli/src/main.rs:261`: add `donation_rate: None,` and `donor_email: None,`. (Task 6 fills these in.)

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p argos-core && cargo build -p argos-cli`
Expected: builds clean.

- [ ] **Step 4: Commit**

```bash
git add crates/zeck-core/src/models.rs crates/zeck-core/src/service.rs gui/src-tauri/src/commands.rs crates/zeck-cli/src/main.rs
git commit -m "feat(core): add donation fields to sweep request and proposal models"
```

---

## Task 3: Donation split in the dry-run proposal (TDD)

**Files:**
- Modify: `crates/zeck-core/src/service.rs` (`propose_sweep` ~207, `build_sweep_proposal` ~288, shielded sweep `ProposedTx` push ~393)

**Context:** `build_sweep_proposal` is an *estimate* — per shielded account it computes `net_received_for_account = shielded_available - sweep_fee`. That `net_received_for_account` is the send amount the donation splits. The builder currently has no network; `propose_sweep` must pass it so the proposal can skip donations on testnet.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `service.rs` (follow the existing `proposal_*` test style; reuse whatever `ScanProgress`/`AccountBalancePreview` fixture builders those tests already use):

```rust
#[test]
fn proposal_splits_donation_out_of_shielded_send_on_mainnet() {
    // Build a progress fixture with one shielded account of, say, 3_000_000
    // zats (mirror an existing proposal test's setup).
    let progress = /* existing helper producing a 3_000_000-zat shielded account */;
    let request = SweepRequest {
        destination: VALID_TEST_UA.to_owned(),
        memo: None,
        max_fee_zatoshis: None,
        donation_rate: Some(0.10),
        donor_email: Some("donor@example.com".to_owned()),
    };
    // network = Mainnet, donation feature assumed enabled in this test
    let proposal = build_sweep_proposal(&progress, request, ZeckNetwork::Mainnet).unwrap();
    let sweep = proposal
        .transactions
        .iter()
        .find(|t| t.kind == ProposedTxKind::SweepShielded)
        .unwrap();
    // donation == 10% of net send amount, recorded on the tx and in the total
    assert!(sweep.donation_zatoshis > 0);
    assert_eq!(proposal.total_donation_zatoshis, sweep.donation_zatoshis);
    // remainder + donation + fee still accounts for the full shielded balance
    assert_eq!(
        sweep.net_zatoshis + sweep.fee_zatoshis,
        sweep.gross_zatoshis
    );
}

#[test]
fn proposal_skips_donation_on_testnet() {
    let progress = /* same 3_000_000-zat fixture */;
    let request = SweepRequest {
        destination: VALID_TEST_UA.to_owned(),
        memo: None,
        max_fee_zatoshis: None,
        donation_rate: Some(0.10),
        donor_email: None,
    };
    let proposal = build_sweep_proposal(&progress, request, ZeckNetwork::Testnet).unwrap();
    assert_eq!(proposal.total_donation_zatoshis, 0);
}
```

> If the donation feature is gated on a non-empty `DONATION_ADDRESS` (empty by default), these tests must inject a test address. Cleanest approach: have `build_sweep_proposal` read the address via a small indirection you can override in tests — e.g. pass the address in, defaulting to `donation::DONATION_ADDRESS` from `propose_sweep`. Add an `donation_address: &str` parameter to `build_sweep_proposal` so tests pass `VALID_TEST_UA` and production passes the constant. Adjust the test calls accordingly.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p argos-core proposal_splits_donation 2>&1 | tail -20`
Expected: FAIL (donation_zatoshis is 0 / signature mismatch).

- [ ] **Step 3: Implement**

1. Change signature: `fn build_sweep_proposal(progress: &ScanProgress, request: SweepRequest, network: ZeckNetwork, donation_address: &str) -> ZeckResult<SweepProposal>`.
2. At the top, validate: `donation::validate_donation_rate(request.donation_rate)?;` and `donation::validate_donor_email(request.donor_email.as_deref())?;`.
3. Compute an effective address: on `ZeckNetwork::Testnet`, force donations off by treating the address as empty:
   ```rust
   let effective_donation_address = if matches!(network, ZeckNetwork::Testnet) { "" } else { donation_address };
   ```
4. In the shielded-sweep branch, after computing `net_received_for_account`, split it:
   ```rust
   let donation_zatoshis = donation::donation_for_send_amount(
       effective_donation_address,
       request.donation_rate,
       net_received_for_account,
   );
   ```
   Keep `net_zatoshis = net_received_for_account` (still the total leaving the wallet to user+donation), but record `donation_zatoshis` on the `ProposedTx`. Add `total_donation_zatoshis += donation_zatoshis`.

   > Note: the estimate's fee (`MINIMUM_FEE`) does not model the extra output. That is acceptable for the *displayed estimate*; the authoritative fee is computed at execution (Task 4). Keep the invariant `net_zatoshis + fee_zatoshis == gross_zatoshis` for the estimate.
5. Set `donation_zatoshis` on the pushed shielded `ProposedTx` and `total_donation_zatoshis` on the returned `SweepProposal`.
6. Update `propose_sweep` (and the `execute_sweep` precheck call at ~236) to fetch network + pass the constant:
   ```rust
   let network = self.session(handle).await?.runtime.network; // confirm field path
   build_sweep_proposal(&progress, request, network, donation::DONATION_ADDRESS)
   ```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p argos-core 2>&1 | tail -25`
Expected: new tests PASS, existing proposal tests still PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/zeck-core/src/service.rs
git commit -m "feat(core): split donation out of dry-run sweep proposal estimate"
```

---

## Task 4: Two-pass donation build in execution

**Files:**
- Modify: `crates/zeck-core/src/service.rs` (`execute_sweep_for_session` ~442, `execute_send_max_step` ~688)

**Context:** `execute_send_max_step` currently calls `propose_send_max_transfer` (single destination, auto fee/change). To carve out a donation we use the two-pass approach from the spec. The pure split (`donation::donation_for_send_amount`) is reused so proposal and execution agree.

- [ ] **Step 1: Thread donation context into the send step**

Add `donation_address: &str`, `donation_rate: Option<f64>`, `donation_memo: Option<MemoBytes>` to `SweepStepCtx` (or pass as args to `execute_send_max_step`). In `execute_sweep_for_session`:
- After deriving `memo_bytes`, validate the request fields (`validate_donation_rate`, `validate_donor_email`) and build the donation memo once: `let donation_memo = Some(MemoBytes::from_bytes(donation::donation_memo_body(request.donor_email.as_deref()).as_bytes())?);`
- Compute `effective_donation_address` = `""` on testnet else `donation::DONATION_ADDRESS`; parse it once to a `ZcashAddress` only if non-empty.

- [ ] **Step 2: Implement the two-pass build in `execute_send_max_step`**

Replace the single `propose_send_max_transfer` → `create_proposed_transactions` flow with:

```rust
// Pass 1 — measure: build the send-max proposal to learn the spendable
// amount and its fee, without broadcasting.
let max_proposal = propose_send_max_transfer::<_, _, _, Infallible>(
    &mut wallet_db, &consensus_network(ctx.network), tracked_account.wallet_account_id,
    &[ShieldedProtocol::Sapling, ShieldedProtocol::Orchard], &StandardFeeRule::Zip317,
    destination_address.clone(), memo_bytes.clone(), MaxSpendMode::MaxSpendable,
    ConfirmationsPolicy::MIN,
)?;
let send_max_fee = proposal_fee_zatoshis(&max_proposal)?;
let send_amount = /* amount routed to destination in max_proposal */;

let donation = donation::donation_for_send_amount(ctx.donation_address, ctx.donation_rate, send_amount);

let (proposal, fee_zatoshis) = if donation == 0 {
    (max_proposal, send_max_fee)            // unchanged behavior
} else {
    // Pass 2 — split: fixed two-payment transfer. The extra output raises the
    // ZIP-317 fee by one marginal action; absorb that delta into the remainder.
    // Build a candidate transfer to measure fee_two_output, then set
    // remainder = send_amount - donation - (fee_two_output - send_max_fee).
    // Use propose_transfer with a Request of two Payments:
    //   - donation -> ctx.donation_address (donation_memo)
    //   - remainder -> destination_address (memo_bytes)
    let split_proposal = propose_transfer(/* two-payment request, see below */)?;
    let fee_two_output = proposal_fee_zatoshis(&split_proposal)?;
    (split_proposal, fee_two_output)
};
```

Implementation notes for the planner/executor:
- Extract `send_amount` from the send-max proposal's single payment output value (inspect the `Proposal` step's transaction request / balance). If the API exposes it as `total - fee`, compute `send_amount` from the proposal balance minus `send_max_fee`.
- Build the two-payment request with `zcash_client_backend::data_api::wallet::input_selection`/`zip321::TransactionRequest` exactly as other `propose_transfer` call sites in the codebase do (search for `propose_transfer(` to copy the input-selector + change-strategy setup; mirror `execute_shielding_step`'s `GreedyInputSelector` + `SingleOutputChangeStrategy`).
- Because both payments are fixed and sum to `send_amount - (fee_two_output - send_max_fee)`, there is no change output, preserving the full-spend invariant. If the recomputed `fee_two_output` differs from the first estimate after fixing amounts, iterate once (recompute remainder with the measured fee). Pin a single deterministic rounding rule: if rounding leaves a sub-dust discrepancy, reduce `donation` by the discrepancy so the destination receives exactly the intended remainder.
- `enforce_max_fee` against `prior + fee_zatoshis` as today.
- Feed `proposal` into the existing `create_proposed_transactions` + `broadcast_transactions` calls unchanged. Return `fee_zatoshis`.

- [ ] **Step 3: Build + existing tests**

Run: `cargo build -p argos-core && cargo test -p argos-core 2>&1 | tail -20`
Expected: builds; existing tests pass. (The two-pass path is exercised end-to-end in manual verification — see Task 9 — because it needs a funded wallet DB; the split math itself is unit-tested in Task 1.)

- [ ] **Step 4: Run clippy (both matter per CLAUDE.md)**

Run: `cargo clippy -p argos-core --all-targets -- -D warnings`
Expected: no warnings. Watch the 7-argument lint on `execute_send_max_step` — if it trips, bundle the donation params into a small struct like the existing `ShieldedProbeKeys` pattern.

- [ ] **Step 5: Commit**

```bash
git add crates/zeck-core/src/service.rs
git commit -m "feat(core): build donation output via two-pass send-max split in execution"
```

---

## Task 5: Thread donation fields through Tauri commands

**Files:**
- Modify: `gui/src-tauri/src/commands.rs` (`propose_sweep` ~141, `execute_sweep` ~170; add `donation_address` command)

- [ ] **Step 1: Add params to both commands**

Add `donation_rate: Option<f64>` and `donor_email: Option<String>` to the `propose_sweep` and `execute_sweep` command signatures, and set them in the `SweepRequest { .. }` literals (replacing the `None` placeholders from Task 2).

- [ ] **Step 2: Add a command exposing the baked address**

```rust
#[tauri::command]
pub fn donation_address() -> String {
    argos_core::DONATION_ADDRESS.to_owned()
}
```

Register it in the `tauri::generate_handler!` macro in `gui/src-tauri/src/main.rs` (~line 14) — add `commands::donation_address` to the list.

- [ ] **Step 3: Build**

Run: `cd gui/src-tauri && cargo build`
Expected: builds clean.

- [ ] **Step 4: Commit**

```bash
git add gui/src-tauri/src/commands.rs gui/src-tauri/src/*.rs
git commit -m "feat(gui): thread donation fields through Tauri sweep commands"
```

---

## Task 6: CLI flags

**Files:**
- Modify: `crates/zeck-cli/src/main.rs` (sweep subcommand args + `SweepRequest` at ~261)

- [ ] **Step 1: Add flags**

Add to the relevant `clap` sweep arguments (mirror the existing `memo` / `max_fee` flag definitions):

```rust
/// Fraction of recovered funds to donate to the project (e.g. 0.10). Omit to skip.
#[arg(long)]
donation_rate: Option<f64>,
/// Email placed in the donation memo for an off-chain receipt.
#[arg(long)]
donor_email: Option<String>,
```

Set `donation_rate` and `donor_email` in the `SweepRequest` at ~261.

- [ ] **Step 2: Build + smoke test help**

Run: `cargo run -p argos-cli -- recover --help 2>&1 | grep -i donation`
Expected: both flags listed.

- [ ] **Step 3: Commit**

```bash
git add crates/zeck-cli/src/main.rs
git commit -m "feat(cli): add --donation-rate and --donor-email flags"
```

---

## Task 7: Donation form on the Review & Sweep screen (HTML/CSS)

**Files:**
- Modify: `gui/src/index.html` (`data-step="sweep"` section, before the Execute button ~443)
- Modify: `gui/src/styles.css`

- [ ] **Step 1: Add the form markup**

Inside the `data-step="sweep"` section, above the execute controls, add:

```html
<div class="donate-form report-card" id="donate-form">
  <label class="checkbox-row">
    <input type="checkbox" id="donate-enabled" checked />
    <span>Donate a share of recovered funds to support Argos</span>
  </label>
  <div class="donate-fields" id="donate-fields">
    <label class="field">
      <span>Donation percentage</span>
      <input id="donate-rate" type="number" min="0" max="100" step="1" value="10" />
    </label>
    <label class="field">
      <span>Email for receipt (optional)</span>
      <input id="donate-email" type="email" placeholder="you@example.com" />
    </label>
    <p class="status-line" id="donate-amount-preview"></p>
  </div>
</div>
```

- [ ] **Step 2: Add styles** (reuse existing `report-card`, `field`, `status-line`; add only what's new)

```css
.donate-form { border-left: 3px solid var(--accent); }
.checkbox-row { display: flex; align-items: center; gap: 8px; }
.donate-fields { margin-top: 10px; }
.donate-fields[hidden] { display: none; }
```

- [ ] **Step 3: Verify it renders** — open the app (or `/run`) to the sweep screen; confirm the form appears. No commit yet (wired in Task 8); or commit markup now:

```bash
git add gui/src/index.html gui/src/styles.css
git commit -m "feat(gui): add donation form markup to sweep review screen"
```

---

## Task 8: Wire the donation form (main.js)

**Files:**
- Modify: `gui/src/main.js` (sweep proposal flow ~758-800, `renderSweepProposal` ~786)

- [ ] **Step 1: Collect form values into the propose/execute invokes**

Where `invoke("propose_sweep", { ... })` and the execute invoke are called, add:

```js
const donateEnabled = $("donate-enabled").checked;
const ratePct = parseFloat($("donate-rate").value);
const donationRate = donateEnabled && ratePct > 0 ? ratePct / 100 : null;
const donorEmail = donateEnabled ? ($("donate-email").value.trim() || null) : null;
```

Pass `donationRate` and `donorEmail` (camelCase → Tauri maps to snake_case args) into both invokes.

- [ ] **Step 2: Live preview from the returned proposal**

In `renderSweepProposal(proposal)`, after rendering rows, show the donation total:

```js
const donated = proposal.total_donation_zatoshis || 0;
$("donate-amount-preview").textContent = donated > 0
  ? `Donation: ${formatZec(donated)} ZEC (net to you: ${formatZec(proposal.net_received_zatoshis - donated)} ZEC)`
  : (donateEnabledAtProposeTime ? "Donation below the minimum threshold — skipped." : "");
```

Use the existing zat→ZEC formatter — on the base branch this is `fmt` (main.js:29); confirm the helper name on `feat/donate-flow` and use whatever renders `net_zatoshis` elsewhere.

- [ ] **Step 3: Toggle field visibility**

```js
$("donate-enabled").addEventListener("change", () => {
  $("donate-fields").hidden = !$("donate-enabled").checked;
});
```

- [ ] **Step 4: Hide the whole form on testnet**

The network lives at `state.scanConfig.network` (see the existing `state.scanConfig?.network` usage around main.js:960), **not** `state.network`. Hide `#donate-form` when testnet, after the scan config is set:

```js
if (state.scanConfig?.network === "testnet") $("donate-form").hidden = true;
```

- [ ] **Step 5: Verify** — run the app on the test seed, reach the sweep screen, toggle the checkbox, edit the percentage, confirm the preview updates after re-proposing. Confirm testnet hides the form.

- [ ] **Step 6: Commit**

```bash
git add gui/src/main.js
git commit -m "feat(gui): wire donation form to sweep proposal and execution"
```

---

## Task 9: Repoint the donate-anytime overlay at the baked address + manual verification

**Files:**
- Modify: `gui/src/main.js` (the `initDonate` block from PR #66)

- [ ] **Step 1: Populate the overlay address from the backend constant**

Replace the PR #66 `DONATE_ZEC_ADDRESS` frontend constant with a call to the new command at startup:

```js
(async function initDonate() {
  const addr = await invoke("donation_address");
  if (addr) {
    $("donate-address").textContent = addr;
    $("copy-donate-address").disabled = false;
  }
  // empty → keep "Address coming soon" + disabled copy (PR #66 behavior)
})();
```

Update the copy handler to read `donate-address`'s textContent rather than the removed constant.

- [ ] **Step 2: Manual end-to-end verification (the part unit tests can't cover)**

With a **non-empty** test `DONATION_ADDRESS` temporarily set (a valid mainnet UA you control), against a funded test wallet or mainnet dry-run:
- [ ] Propose a sweep with 10% donation → proposal shows a non-zero donation total and reduced net-to-you.
- [ ] Below-threshold account → no donation line, behavior identical to today.
- [ ] Execute (dry-run / small real amount): broadcast tx has two shielded outputs; donation output decrypts (with the donation viewing key) to memo = tag + email.
- [ ] Confirm `donation + remainder + fee == spendable` for the send leg.
- [ ] Testnet → no donation output, form hidden.
- [ ] Overlay shows the baked address; copy works.

> Expected, not a bug: the proposal estimate computes the donation on the `MINIMUM_FEE` estimate while execution uses the real ZIP-317 fee, so an account whose donation sits right at the 100,000-zat threshold may show a donation in the proposal but skip it at execution (or vice versa). Don't chase this during verification.

Revert `DONATION_ADDRESS` to empty (or to the real address once provided) before final commit.

- [ ] **Step 3: Commit**

```bash
git add gui/src/main.js
git commit -m "feat(gui): source donate-anytime overlay address from baked constant"
```

---

## Final checks

- [ ] `cargo test --workspace 2>&1 | tail -20` — all green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- [ ] Confirm `DONATION_ADDRESS` is set to the intended value (empty to ship dormant, or the real shielded UA to activate) before opening/refreshing PR #66.
- [ ] Push to `feat/donate-flow`; update the PR #66 description to reflect the in-sweep flow.
</content>
