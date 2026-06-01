# Donation Tip-Checkout Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Argos's bare donation percentage input with a story-led, payment-terminal-style tip checkout on the sweep screen, and align the donation copy across the completion card and donate overlay.

**Architecture:** Frontend-only change in the Tauri GUI (`gui/src/`). Preset chips (5/10/20% + Custom) write into the *existing* `#donate-rate` value and `#donate-enabled` checkbox so the Rust backend, the `propose_sweep` contract, `donationParamsFromForm()`, and proposal-refresh logic are all untouched. Per-chip ZEC amounts are client-side estimates; only the selected rate is ever sent to the backend.

**Tech Stack:** Vanilla HTML/CSS/JS (no bundler, `withGlobalTauri`), served by Tauri v2. No JS test framework exists in this project and none is added (dependency policy) — verification is manual in `tauri dev`.

**Spec:** `docs/superpowers/specs/2026-06-01-donation-tip-checkout-design.md`

---

## Testing approach (read first)

This frontend has no automated JS test harness and the project intentionally avoids adding one. Each task therefore ends with a **manual verification block** run against `npm run dev` (Tauri dev) in `gui/`, using the BIP-39 test seed from `CLAUDE.md` with a **mainnet** config (the donation form is hidden on testnet). Where a discrete pure function is introduced (`estimateDonationZat`), it is written to be trivially correct and is exercised through the UI. Commit after each task.

To reach the sweep screen for verification: launch `npm run dev`, enter the test seed, a mainnet destination UA, run a scan to completion (or far enough to enable Review & Sweep), and click **Review & Sweep**. Keep this dev instance open across tasks.

---

## File Structure

- **`gui/src/styles.css`** — new preset-chip / skip-link / story-copy styles. Responsibility: visual treatment only.
- **`gui/src/index.html`** — markup for (a) the redesigned sweep tip card, (b) refreshed completion card copy, (c) refreshed donate-overlay copy.
- **`gui/src/main.js`** — preset/skip interaction handlers, `estimateDonationZat` helper, reset-on-new-run update. `donationParamsFromForm()` stays byte-for-byte unchanged.

---

## Task 1: Preset-chip, skip-link, and story-copy CSS

**Files:**
- Modify: `gui/src/styles.css` (donate styles live near lines 631–645 and 965–969)

- [ ] **Step 1: Add the new styles**

Append after the existing `.donate-fields[hidden]` rule (around line 969) in `gui/src/styles.css`:

```css
/* ─── Tip-checkout (sweep screen) ─────────────────────────────────────── */
.donate-eyebrow {
  text-transform: uppercase;
  letter-spacing: 0.14em;
  font-size: 0.7rem;
  color: var(--muted);
  margin: 0 0 6px;
}
.donate-heading {
  margin: 0 0 8px;
  font-size: 1.15rem;
}
.donate-story {
  margin: 0 0 14px;
  color: var(--muted);
  font-size: 0.9rem;
  line-height: 1.55;
}
.preset-row {
  display: flex;
  gap: 9px;
  margin-bottom: 10px;
}
.preset-chip {
  flex: 1;
  text-align: center;
  border: 1.5px solid var(--accent-soft);
  border-color: rgba(31, 107, 86, 0.35);
  border-radius: 13px;
  padding: 11px 4px;
  background: var(--panel-strong);
  cursor: pointer;
  font: inherit;
  color: inherit;
  transition: background 0.12s ease, border-color 0.12s ease;
}
.preset-chip:hover {
  border-color: var(--accent);
}
.preset-chip .preset-pct {
  font-size: 1.15rem;
  font-weight: 600;
  color: var(--accent);
  display: block;
}
.preset-chip .preset-amt {
  font-size: 0.7rem;
  color: var(--muted);
  margin-top: 3px;
  display: block;
}
.preset-chip[aria-pressed="true"] {
  background: linear-gradient(135deg, #1f6b56, #154a3c);
  border-color: #154a3c;
}
.preset-chip[aria-pressed="true"] .preset-pct,
.preset-chip[aria-pressed="true"] .preset-amt {
  color: var(--panel-strong);
}
.donate-skip {
  background: none;
  border: none;
  padding: 0;
  margin-top: 12px;
  font: inherit;
  font-size: 0.8rem;
  color: var(--muted);
  text-decoration: underline;
  cursor: pointer;
}
.donate-collapsed .preset-row,
.donate-collapsed .donate-fields,
.donate-collapsed #donate-email-field,
.donate-collapsed #donate-amount-preview {
  display: none;
}
```

- [ ] **Step 2: Verify CSS parses (no app change yet)**

Run: `cd gui && node -e "const c=require('fs').readFileSync('src/styles.css','utf8'); const o=(c.match(/{/g)||[]).length, x=(c.match(/}/g)||[]).length; if(o!==x) throw new Error('brace mismatch '+o+'/'+x); console.log('braces balanced',o)"`
Expected: `braces balanced <N>` (no throw)

- [ ] **Step 3: Commit**

```bash
git add gui/src/styles.css
git commit -m "style(gui): add tip-checkout preset-chip styles"
```

---

## Task 2: Redesign the sweep tip-card markup

**Files:**
- Modify: `gui/src/index.html` (current `#donate-form` block, lines 486–502)

**Constraint:** Keep these element IDs so `main.js` / backend wiring still works: `donate-form`, `donate-enabled`, `donate-fields`, `donate-rate`, `donate-email`, `donate-amount-preview`. `#donate-enabled` stays in the DOM (now visually hidden) because it remains the source of truth for "donation on/off".

- [ ] **Step 1: Replace the `#donate-form` block**

Replace lines 486–502 of `gui/src/index.html` (the entire `<div class="donate-form report-card" id="donate-form">…</div>`) with:

```html
          <div class="donate-form report-card" id="donate-form">
            <p class="donate-eyebrow">Support Argos</p>
            <h3 class="donate-heading">Help us keep doing this</h3>
            <p class="donate-story">
              This effort was only possible through public donations. If Argos
              helped you recover your ZEC, Sovright would appreciate a small
              donation to continue doing work like this.
            </p>
            <!-- Source of truth for on/off; toggled by the chips/skip link, not shown directly. -->
            <input type="checkbox" id="donate-enabled" checked hidden />
            <div class="preset-row" id="donate-presets" role="group" aria-label="Donation amount">
              <button type="button" class="preset-chip" data-pct="5" aria-pressed="false">
                <span class="preset-pct">5%</span><span class="preset-amt"></span>
              </button>
              <button type="button" class="preset-chip" data-pct="10" aria-pressed="true">
                <span class="preset-pct">10%</span><span class="preset-amt"></span>
              </button>
              <button type="button" class="preset-chip" data-pct="20" aria-pressed="false">
                <span class="preset-pct">20%</span><span class="preset-amt"></span>
              </button>
              <button type="button" class="preset-chip" data-pct="custom" aria-pressed="false">
                <span class="preset-pct">Custom</span><span class="preset-amt">%</span>
              </button>
            </div>
            <div class="donate-fields" id="donate-fields" hidden>
              <label class="field">
                <span>Donation percentage</span>
                <input id="donate-rate" type="number" min="0" max="99" step="1" value="10" />
              </label>
            </div>
            <label class="field" id="donate-email-field">
              <span>Email for receipt (optional)</span>
              <input id="donate-email" type="email" placeholder="you@example.com" />
            </label>
            <p class="status-line" id="donate-amount-preview" style="margin-top:8px"></p>
            <button type="button" class="donate-skip" id="donate-skip">No thanks, skip donation</button>
          </div>
```

Notes on the change:
- `#donate-rate` moves inside `#donate-fields` and `#donate-fields` now starts `hidden` (it is revealed only by the Custom chip), where previously it was always shown. Its default `value="10"` is unchanged.
- `#donate-enabled` becomes a hidden checkbox (still `checked` by default).
- A new `#donate-presets`, `#donate-skip`, and `#donate-email-field` are introduced.

- [ ] **Step 2: Verify HTML well-formedness**

Run: `cd gui && node -e "const h=require('fs').readFileSync('src/index.html','utf8'); for(const id of ['donate-form','donate-enabled','donate-fields','donate-rate','donate-email','donate-amount-preview','donate-presets','donate-skip']){ if(!h.includes('id=\"'+id+'\"')) throw new Error('missing #'+id);} console.log('all donate IDs present')"`
Expected: `all donate IDs present`

- [ ] **Step 3: Commit**

```bash
git add gui/src/index.html
git commit -m "feat(gui): story-led preset markup for sweep tip card"
```

(The card is not yet wired — chips do nothing until Task 3. Visual-only check is fine here.)

---

## Task 3: Wire preset chips, skip link, and per-chip estimates

**Files:**
- Modify: `gui/src/main.js` — add `estimateDonationZat` + handlers near the existing donate listeners (lines 860–865); update `renderSweepProposal` per-chip amounts (around lines 924–939); update reset-on-new-run (lines 1197–1201).

**Key invariant:** `donationParamsFromForm()` (lines 806–813) is NOT modified. Chips/skip only mutate `#donate-rate.value` and `#donate-enabled.checked`, then call `maybeRefreshProposal()`.

- [ ] **Step 1: Add the estimate helper + chip/skip wiring**

Replace the existing block at lines 860–865:

```javascript
$("donate-enabled").addEventListener("change", () => {
  $("donate-fields").hidden = !$("donate-enabled").checked;
  maybeRefreshProposal();
});
$("donate-rate").addEventListener("change", maybeRefreshProposal);
// email does not affect amounts; it's read fresh at propose/execute time, no re-propose needed
```

with:

```javascript
// Client-side per-chip estimate: donation as a % of what you'd net without a
// donation. Only the *selected* rate is ever sent to the backend; the chips
// just preview amounts so we don't fan out three propose_sweep calls.
function estimateDonationZat(pct) {
  const p = state.sweepProposal;
  if (!p) return null;
  const base = (p.net_received_zatoshis || 0) + (p.total_donation_zatoshis || 0);
  return Math.round((base * pct) / 100);
}

// Reflect a chosen percentage into the source-of-truth fields and re-propose.
function selectDonationPreset(pctValue) {
  const custom = pctValue === "custom";
  $("donate-enabled").checked = true;
  $("donate-form").classList.remove("donate-collapsed");
  $("donate-fields").hidden = !custom;
  if (!custom) $("donate-rate").value = String(pctValue);
  document.querySelectorAll("#donate-presets .preset-chip").forEach((chip) => {
    chip.setAttribute(
      "aria-pressed",
      String(chip.dataset.pct === String(pctValue)),
    );
  });
  maybeRefreshProposal();
}

document.querySelectorAll("#donate-presets .preset-chip").forEach((chip) => {
  chip.addEventListener("click", () => selectDonationPreset(chip.dataset.pct));
});

// Custom field: keep its chip highlighted while typing; re-propose on change.
$("donate-rate").addEventListener("input", () => {
  document.querySelectorAll("#donate-presets .preset-chip").forEach((chip) => {
    chip.setAttribute("aria-pressed", String(chip.dataset.pct === "custom"));
  });
});
$("donate-rate").addEventListener("change", maybeRefreshProposal);

// Skip toggles the donation off (collapse) / back on (default 10%). The
// eyebrow + story copy stay visible either way.
$("donate-skip").addEventListener("click", () => {
  const turningOff = $("donate-enabled").checked;
  if (turningOff) {
    $("donate-enabled").checked = false;
    $("donate-form").classList.add("donate-collapsed");
    document.querySelectorAll("#donate-presets .preset-chip").forEach((chip) => {
      chip.setAttribute("aria-pressed", "false");
    });
    $("donate-skip").textContent = "Changed your mind? Add a donation";
    maybeRefreshProposal();
  } else {
    $("donate-skip").textContent = "No thanks, skip donation";
    selectDonationPreset("10"); // re-enables, un-collapses, and re-proposes
  }
});
```

Final shape of the replacement region: `estimateDonationZat`, `selectDonationPreset`, the chip `click` loop, the `#donate-rate` `input` + `change` listeners, and the single `#donate-skip` handler. (No `#donate-enabled` change listener is needed anymore — it is hidden and only mutated programmatically.)

- [ ] **Step 2: Populate per-chip amounts in `renderSweepProposal`**

In `renderSweepProposal`, immediately after the line `$("donate-form").hidden = !state.donationEnabled || state.scanConfig?.network === "testnet";` (line 924), insert:

```javascript
  // Fill each preset chip's "≈ X ZEC" estimate from the current proposal.
  document.querySelectorAll("#donate-presets .preset-chip").forEach((chip) => {
    if (chip.dataset.pct === "custom") return;
    const est = estimateDonationZat(parseFloat(chip.dataset.pct));
    chip.querySelector(".preset-amt").textContent =
      est == null ? "" : `≈ ${fmt(est)}`;
  });
```

The existing `donate-amount-preview` if/else branch (lines 925–939) is unchanged — it still shows the authoritative selected-amount string and preserves the ZIP-317 sub-threshold caveat.

- [ ] **Step 3: Update reset-on-new-run**

Replace lines 1197–1201:

```javascript
  $("donate-enabled").checked = true;
  $("donate-rate").value = "10";
  $("donate-email").value = "";
  $("donate-fields").hidden = false;
  setStatus("donate-amount-preview", "", "");
```

with:

```javascript
  $("donate-enabled").checked = true;
  $("donate-rate").value = "10";
  $("donate-email").value = "";
  $("donate-fields").hidden = true;
  $("donate-form").classList.remove("donate-collapsed");
  $("donate-skip").textContent = "No thanks, skip donation";
  document.querySelectorAll("#donate-presets .preset-chip").forEach((chip) => {
    chip.setAttribute("aria-pressed", String(chip.dataset.pct === "10"));
  });
  setStatus("donate-amount-preview", "", "");
```

- [ ] **Step 4: Syntax check**

Run: `cd gui && node --check src/main.js`
Expected: no output (exit 0)

- [ ] **Step 5: Manual verification in the app**

Run: `cd gui && npm run dev`
Then drive to the sweep screen (test seed, mainnet destination) and confirm:
- The **10%** chip is highlighted by default; each of 5/10/20 shows `≈ <ZEC>` beneath it; the amount-preview line shows the authoritative selected donation.
- Clicking **5%** / **20%** highlights that chip and the sweep table's Donation column + summary update (proposal re-fetched).
- Clicking **Custom** reveals the number field; typing a value keeps Custom highlighted and updates amounts on change.
- Clicking **No thanks, skip donation** collapses the chips, sets the table donation to 0, and the link becomes "Changed your mind? Add a donation"; clicking it again restores 10%.
- Email field still present; entering an email does not trigger a re-propose.

- [ ] **Step 6: Commit**

```bash
git add gui/src/main.js
git commit -m "feat(gui): wire tip-checkout presets, skip, and per-chip estimates"
```

---

## Task 4: Refresh the completion-screen card copy

**Files:**
- Modify: `gui/src/index.html` (`.donate-card` in the `complete` section, lines 520–524)

- [ ] **Step 1: Update the copy**

Replace lines 521–522 (the `<h3>` and `<p>` inside `.donate-card`):

```html
            <h3>Support Argos</h3>
            <p>Argos is free and open source. If it helped you recover your ZEC, consider making a donation to support ongoing development.</p>
```

with:

```html
            <h3>Help us keep doing this</h3>
            <p>This effort was only possible through public donations. If Argos helped you recover your ZEC, Sovright would appreciate a small donation to continue doing work like this.</p>
```

(The `#complete-open-donate` button below is unchanged.)

- [ ] **Step 2: Manual verification**

In the running dev app, reach the completion screen (or temporarily inspect the markup) and confirm the card reads with the new heading/body and the **Donate** button still opens the overlay.

- [ ] **Step 3: Commit**

```bash
git add gui/src/index.html
git commit -m "copy(gui): align completion-card donation copy with tip checkout"
```

---

## Task 5: Refresh the donate-overlay copy

**Files:**
- Modify: `gui/src/index.html` (`#donate-overlay`, the intro paragraph at lines 58–59)

- [ ] **Step 1: Update the overlay intro**

Replace lines 58–59:

```html
              <h3>Support Argos development</h3>
              <p>Argos is free and open source. If it helped you recover your ZEC, a donation supports ongoing development and maintenance.</p>
```

with:

```html
              <h3>Help us keep doing this</h3>
              <p>This effort was only possible through public donations. If Argos helped you recover your ZEC, Sovright would appreciate a small donation to continue doing work like this.</p>
```

(The address/copy block below is unchanged.)

- [ ] **Step 2: Manual verification**

In the running dev app, click the top-bar **♥ Donate** button and confirm the overlay shows the new copy above the address block, and Copy still works.

- [ ] **Step 3: Commit**

```bash
git add gui/src/index.html
git commit -m "copy(gui): align donate-overlay copy with tip checkout"
```

---

## Final verification checklist

- [ ] On **testnet** config, the sweep `#donate-form` is hidden (existing gating unaffected).
- [ ] A fresh run (start over) resets the tip card to default-on 10% with chips/skip text reset and Custom field hidden.
- [ ] `node --check gui/src/main.js` passes and CSS braces balance.
- [ ] All three touchpoints (sweep card, completion card, overlay) read in one consistent voice.
- [ ] No Rust/CLI files were modified (`git diff --name-only main` shows only `gui/src/*` and docs).
