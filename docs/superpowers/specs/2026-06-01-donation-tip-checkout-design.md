# Donation tip-checkout redesign — design

**Date:** 2026-06-01
**Status:** Approved (brainstorming)
**Component:** `gui/` (Tauri frontend only — no Rust/backend changes)

## Goal

Make Argos's in-app donation ask feel as compelling as a payment-terminal tip
prompt, so users who just recovered funds *want* to leave a share. Replace the
current bare percentage input with a preset-driven, story-led tip checkout, and
align the copy across all three donation touchpoints.

## Background

Argos asks for donations in three places today:

1. **Sweep review screen** (`#donate-form` in `gui/src/index.html`) — the "tip
   moment". A donation rides *inside* the sweep transaction as a percentage of
   recovered funds. Currently a plain number input (`#donate-rate`, default 10)
   plus an enable checkbox (`#donate-enabled`) and an optional email field
   (`#donate-email`).
2. **Donate overlay** (`#donate-overlay`) — a standalone copy-the-address panel
   reached from the top bar.
3. **Completion screen** (`.donate-card` in the `complete` section) — a "Support
   Argos" card with a button that opens the overlay.

The donation mechanism itself works well; the weakness is presentation. A
free-form number field neither anchors an amount nor communicates *why* to give.

### Mechanism that MUST stay unchanged

`donationParamsFromForm()` in `gui/src/main.js` reads three things:
`#donate-enabled` (checkbox), `#donate-rate` (percentage number), `#donate-email`.
It produces `{ donationRate: pct/100 | null, donorEmail }`, which feeds
`maybeRefreshProposal` and the execute path. The Rust backend computes
`total_donation_zatoshis` per transaction. The redesign keeps these three input
values as the source of truth so the backend contract and proposal refresh logic
are untouched. The form is already hidden on testnet and when no donation
address is configured (`state.donationEnabled`); that gating stays.

## Design

### 1. Sweep-screen tip card (primary change)

Replace the number-input UI with a story-led preset checkout:

- **Copy (story-led, "continue this work" framing):**
  - Eyebrow: `Support Argos`
  - Heading: `Help us keep doing this`
  - Body: *"This effort was only possible through public donations. If Argos
    helped you recover your ZEC, Sovright would appreciate a small donation to
    continue doing work like this."*
- **Preset row:** three chips — **5% / 10% / 20%**, with **10% pre-selected** as
  the anchor — plus a **Custom** chip. Each preset chip shows the live ZEC amount
  beneath its percentage, computed from the current proposal totals.
- **Custom chip:** reveals the existing numeric input for an arbitrary 0–99%
  value (backend already supports this range).
- **Amount line:** "You keep **X ZEC** · donate **Y ZEC**", reusing the existing
  `#donate-amount-preview` element and proposal totals
  (`total_donation_zatoshis`, `net_received_zatoshis`).
- **Explicit skip:** a quiet "No thanks, skip donation" link that disables the
  donation (sets `#donate-enabled` unchecked) and collapses the presets;
  re-expandable.
- **Email-for-receipt:** kept, but only surfaced once a donation amount is active,
  to reduce first-glance friction.

**State mapping (no backend change):**

| UI action            | Underlying state                                    |
|----------------------|-----------------------------------------------------|
| Select preset (5/10/20) | `#donate-rate` = the %, `#donate-enabled` checked |
| Select Custom        | reveal `#donate-rate` input, `#donate-enabled` checked |
| Click skip link      | `#donate-enabled` unchecked, presets collapsed      |
| Re-enable            | restore last preset (default 10%), `#donate-enabled` checked |

Selecting a preset or editing Custom calls the existing `maybeRefreshProposal`
so amounts stay live. The reset-on-new-run logic (currently forces
`#donate-rate = 10`, enabled) updates to also reset the preset selection to 10%.

### 2. Completion-screen card

Refresh `.donate-card` copy to the same voice; keep its button that opens the
overlay:

- Heading: `Help us keep doing this`
- Body: same "only possible through public donations… continue doing work like
  this" message, past-tense friendly ("If Argos helped you recover your ZEC…").

### 3. Donate overlay

Apply the same story copy to the overlay's intro paragraph (`#donate-overlay`),
above the address/copy block, so the standalone donate path shares one voice.
The address-copy mechanism is unchanged.

## Components / boundaries

- **`gui/src/index.html`** — markup for the preset row, story copy in all three
  touchpoints, skip link, conditional email.
- **`gui/src/main.js`** — preset-chip selection handlers that write into the
  existing `#donate-rate` / `#donate-enabled` fields and call
  `maybeRefreshProposal`; skip/re-enable handlers; updated reset-on-new-run.
  `donationParamsFromForm()` is unchanged.
- **`gui/src/styles.css`** — preset-chip styles (selected = accent gradient),
  story-copy spacing, skip link. Reuse existing tokens (`--accent`,
  `--accent-soft`, `--panel-strong`, radius).

## Implementation notes (resolved ambiguities)

- **Per-chip ZEC amounts are client-side estimates.** Only the *selected* rate is
  ever sent to the backend (`propose_sweep` returns `total_donation_zatoshis` for
  one rate). To show an amount under all three chips without fanning out three
  proposal calls per render, compute each chip's display amount on the frontend as
  an approximation from the recovered/gross total (e.g. `gross * pct`). The
  selected chip's authoritative amount still comes from the proposal via the
  existing `#donate-amount-preview` path. Do NOT issue three `propose_sweep` calls.
- **Preserve the ZIP-317 sub-threshold message.** The current
  `#donate-amount-preview` logic (main.js ~lines 925–936) has a branch for when the
  donation is below the fee threshold and "may be included or skipped". The new
  "You keep X · donate Y" line replaces the *success* branch only; keep the
  sub-threshold caveat branch intact.
- **Reconcile chip show/hide with the existing checkbox listener.** Today
  `#donate-fields` visibility is driven by the `#donate-enabled` change listener
  (main.js ~lines 860–864). The new preset/skip handlers must coordinate with that
  listener (drive a single source of truth) rather than fighting it.

## Out of scope (YAGNI)

- No backend / Rust changes; donation math, receipt email handling, and the
  donation address command are untouched.
- No CLI changes.
- No new dependencies.
- No A/B testing or analytics instrumentation.

## Testing

- Manual GUI walkthrough on **mainnet config** with the BIP-39 test seed: confirm
  presets select, 10% is the default, amounts update live against a proposal,
  Custom reveals the raw input, skip disables the donation (proposal shows zero
  donation), re-enable restores a preset.
- Confirm the tip card stays **hidden on testnet** and when no donation address
  is configured (existing `state.donationEnabled` gating).
- Confirm a fresh run resets to the default-on 10% preset (no carryover from a
  prior sweep).
- Confirm overlay and completion-card copy render and their existing
  buttons/links still work.
