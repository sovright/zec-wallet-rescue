# Donation flow: in-sweep donation with email-memo receipts

Date: 2026-05-27
Status: Approved design, pending implementation plan
Supersedes the passive overlay added in PR #66 (`feat/donate-flow`); the
overlay is retained as a secondary "donate anytime" path.

## Goal

Let a user direct a portion of their recovered funds to the project as part of
the sweep, and optionally attach an email address to the donation so we can send
a receipt. The donation is funded directly from swept funds, so it must be
constructed before/at sweep time while Argos still holds spending authority over
the recovered notes.

## Why before the sweep

After the sweep, funds sit at the user's own recovery destination, which Argos
does not control. The only point at which Argos can build a donation transaction
from recovered funds is during the sweep, while it holds the derived spending
keys. Therefore the donation is an additional output on the sweep transactions
themselves, not a follow-up action.

## On-chain mechanism

The sweep already uses `zcash_client_backend` proposal machinery
(`propose_shielding` for transparent→shielded, `propose_transfer` send-max for
the shielded send to the destination, ZIP-317 fees) in
`crates/zeck-core/src/service.rs`.

The donation is carried as an **extra output on each per-account send
transaction**, not as a separate transaction. Adding one shielded output to a
transaction that is already being built costs roughly one marginal ZIP-317
action (~5,000 zats), far less than a dedicated transaction with its own inputs
and fee.

Per account, in the send step:

1. Compute `donation = round(rate * send_amount)` where `send_amount` is the
   amount that account would otherwise send to the user's destination.
2. **If `donation >= MIN_DONATION_ZATOSHIS`**: build a two-payment
   `propose_transfer`:
   - donation → `DONATION_ADDRESS`, with the donation memo (see below)
   - remainder → user's destination (existing memo behavior unchanged)
3. **Else**: fall back to the current send-max-to-destination with no donation
   output and no extra fee. The threshold gates out dust donations
   automatically, per-transaction.

### Memo format

The donation output carries a memo so all sweep-sourced donations are
identifiable, and so the optional email travels with the donation. The memo is
encrypted to the donation address holder — only the project (holder of the
donation address viewing key) can decrypt it.

```
{DONATION_MEMO_TAG}
{email}        # line omitted when no email provided
```

`DONATION_MEMO_TAG` is a fixed label (e.g. `"Argos sweep donation v1"`). The
combined memo stays well within the 512-byte memo limit.

## Constants (baked into the binary)

Defined in `zeck-core` (e.g. a new `donation.rs` module, or alongside the sweep
code in `service.rs`):

- `DONATION_ADDRESS` — fixed **mainnet unified/shielded** address. Must be
  shielded; transparent outputs cannot carry memos.
- `DONATION_MEMO_TAG` — fixed identifying label string.
- `DEFAULT_DONATION_RATE = 0.10`.
- `MIN_DONATION_ZATOSHIS = 100_000` (0.001 ZEC) — below this, no donation output
  is created for a given transaction.

## Network behavior

There is no testnet donation address. On testnet the donation is skipped
entirely in core (no output is ever added), and the donation form is hidden in
the GUI.

## Request surface

`SweepRequest` gains two optional fields:

- `donation_rate: Option<f64>` — `None` means skip the donation entirely. When
  present it is the fraction of each account's send amount to donate. Validated
  to a sane range (e.g. `0.0 < rate <= 1.0`); out-of-range is rejected.
- `donor_email: Option<String>` — optional. Light format validation only
  (non-empty, contains `@`); empty/None omits the email line from the memo.

`build_sweep_proposal` reflects the donation split so the proposal/summary shows
accurate net-to-user vs. donation amounts before the user executes.

## GUI flow

### Primary path — in the sweep

The donation form lives on the **Review & Sweep** screen
(`data-step="sweep"` in `gui/src/index.html`), above the Execute button,
because the donation is 10% of the *computed* sweep total and the user should
see real numbers.

Form contents:
- "Donate to support Argos" toggle — on by default (skippable).
- Editable rate, defaulting to 10%.
- Live computed donation amount and resulting net-to-you, derived from the
  proposal totals.
- Optional email field labeled for a donation receipt.

Hidden entirely when the active network is testnet.

### Secondary path — donate anytime (QR popup)

The standalone sidebar **♥ Donate** trigger (and the Complete-screen donation
card) from PR #66 are retained, but the overlay is simplified to **pop up a
payment QR code** for the baked-in `DONATION_ADDRESS`. The user scans it with
any wallet to send a manual donation; this path does not move recovered funds.

- The QR encodes a Zcash payment URI (ZIP-321, `zcash:<DONATION_ADDRESS>`) so
  scanning wallets prefill the recipient; the address text + copy button remain
  beneath the QR as a fallback.
- QR generation must work in the no-bundler static frontend. **Open decision:**
  generate the QR (a) client-side via a small vendored QR library, or (b) in
  Rust via a Tauri command (e.g. the `qrcode` crate) returning an SVG/data-URI.
  Option (b) keeps the dependency in Rust and off the frontend; either needs
  explicit dependency approval before adding.

## Error handling

- Donation rate out of range → request validation error, surfaced in the GUI
  before execution.
- Email present but malformed → validation error.
- Donation computed but below threshold → silently no donation output for that
  transaction (expected, not an error).
- Donation address fails to parse (should never happen; baked constant) →
  treated as a build error and surfaced; the sweep does not silently drop the
  donation.

## Testing

Core (`service.rs` unit tests, following existing memo/proposal tests):
- Donation output added when `rate * send_amount >= threshold`; amounts split
  correctly (donation + remainder + fee == account total).
- No donation output when below threshold; behavior identical to today.
- `donation_rate = None` → no donation output.
- Memo body equals tag alone when no email, tag + email line when present.
- Rate out of range and malformed email are rejected.
- Testnet → no donation output regardless of rate.
- Memo stays within 512 bytes with a long email.

GUI: manual test plan — toggle on/off, editing rate updates computed amounts,
email optional, form hidden on testnet, standalone overlay shows baked address.

## Out of scope

- Actually emailing receipts. This design only writes the email into the
  donation memo. A separate off-chain process scans the donation address, reads
  memos (identified by `DONATION_MEMO_TAG`), and sends receipts.
- A testnet donation address.
