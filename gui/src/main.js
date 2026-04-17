function invoke(...args) {
  return window.__TAURI__.core.invoke(...args);
}
function listen(...args) {
  return window.__TAURI__.event.listen(...args);
}

document.addEventListener("DOMContentLoaded", () => {

// ─── State ────────────────────────────────────────────────────────────────────

const state = {
  scanHandle: null,
  lastProgress: null,
  sweepProposal: null,
  destination: null,
  memo: null,
  maxFeeZec: null,
  unlistenProgress: null,
  unlistenComplete: null,
  unlistenDiscovered: null,
};

// ─── Helpers ──────────────────────────────────────────────────────────────────

const $ = (id) => document.getElementById(id);
const fmt = (n) => (Number(n) / 1e8).toFixed(8) + " ZEC";

function phaseLabel(phase) {
  const labels = {
    idle: "Idle",
    validating_seed: "Validating seed…",
    deriving_keys: "Deriving keys…",
    probing_lightwalletd: "Probing lightwalletd…",
    scanning_transparent: "Scanning transparent…",
    scanning_shielded: "Scanning shielded…",
    complete: "Complete ✓",
    cancelled: "Cancelled",
    error: "Error",
  };
  return labels[phase] ?? phase;
}

function setStatus(id, msg, kind) {
  const el = $(id);
  if (!el) return;
  el.textContent = msg;
  el.className = "status-line" + (kind ? ` ${kind}` : "");
}

function fmtSeconds(s) {
  if (s == null) return "—";
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  const r = s % 60;
  return r > 0 ? `${m}m ${r}s` : `${m}m`;
}

function escapeHtml(text) {
  return String(text)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

// ─── Navigation ───────────────────────────────────────────────────────────────

const steps = ["welcome", "seed", "config", "scan", "sweep", "complete"];
let furthestStep = 0; // tracks how far the user has reached

function goTo(step) {
  const stepIdx = steps.indexOf(step);
  if (stepIdx > furthestStep) furthestStep = stepIdx;

  document.querySelectorAll(".screen").forEach((s) => s.classList.remove("active"));
  document.querySelectorAll(".step-list li").forEach((li) => {
    const s = li.dataset.stepIndicator;
    const liIdx = steps.indexOf(s);
    li.classList.remove("active", "complete", "reachable");
    if (liIdx < stepIdx) li.classList.add("complete");
    if (liIdx === stepIdx) li.classList.add("active");
    if (liIdx <= furthestStep) li.classList.add("reachable");
  });
  const screen = document.querySelector(`.screen[data-step="${step}"]`);
  if (screen) screen.classList.add("active");
}

// Make sidebar steps clickable — only allow jumping to already-reached steps
document.querySelectorAll(".step-list li").forEach((li) => {
  li.style.cursor = "pointer";
  li.addEventListener("click", () => {
    const target = li.dataset.stepIndicator;
    const targetIdx = steps.indexOf(target);
    if (targetIdx <= furthestStep) {
      if (target === "config") {
        $("start-scan").disabled = false;
        setStatus("config-status", "", "");
      }
      goTo(target);
    } else {
      // Show a brief tooltip on the step that can't be reached yet
      const prev = steps[targetIdx - 1];
      const prevLabel = li.parentElement.querySelector(`[data-step-indicator="${prev}"]`);
      const originalText = li.textContent;
      li.textContent = "Complete previous steps first";
      setTimeout(() => { li.textContent = originalText; }, 1800);
    }
  });
});

document.querySelectorAll("[data-next]").forEach((btn) => {
  btn.addEventListener("click", () => goTo(btn.dataset.next));
});

document.querySelectorAll("[data-prev]").forEach((btn) => {
  btn.addEventListener("click", () => {
    if (btn.dataset.prev === "config") {
      $("start-scan").disabled = false;
      setStatus("config-status", "", "");
    }
    goTo(btn.dataset.prev);
  });
});

// ─── Step 2: Seed Entry ───────────────────────────────────────────────────────

const seedInput = $("seed-input");
const seedVisibility = $("seed-visibility");
const seedNextBtn = $("seed-next");

seedVisibility.addEventListener("change", () => {
  seedInput.classList.toggle("masked", !seedVisibility.checked);
});

$("seed-validate").addEventListener("click", async () => {
  const words = seedInput.value.trim().toLowerCase().split(/\s+/);
  setStatus("seed-status", "Validating…", "");
  seedNextBtn.disabled = true;
  try {
    await invoke("validate_seed", { words });
    setStatus("seed-status", "✓ Seed phrase is valid.", "success");
    seedNextBtn.disabled = false;
  } catch (err) {
    setStatus("seed-status", `✗ ${err}`, "error");
  }
});

// ─── Step 3: Configuration ────────────────────────────────────────────────────

const SERVER_PRESETS = {
  mainnet: "https://mainnet.lightwalletd.com:9067,https://zec.rocks:443,https://na.zec.rocks:443",
  testnet: "https://lightwalletd.testnet.electriccoin.co:9067",
};

$("network-select").addEventListener("change", () => {
  if ($("server-preset").value === "recommended") {
    $("lightwalletd-url").value = SERVER_PRESETS[$("network-select").value] ?? SERVER_PRESETS.mainnet;
  }
});

$("server-preset").addEventListener("change", () => {
  const preset = $("server-preset").value;
  if (preset === "custom") return;
  const key = preset === "recommended" ? $("network-select").value : preset;
  $("lightwalletd-url").value = SERVER_PRESETS[key] ?? SERVER_PRESETS.mainnet;
});

$("accounts-range").addEventListener("input", () => {
  $("accounts-range-value").textContent = $("accounts-range").value;
});

$("auto-gap-limit").addEventListener("change", () => {
  const auto = $("auto-gap-limit").checked;
  $("gap-limit-row").style.display = auto ? "none" : "block";
  $("accounts-range").disabled = !auto;
  $("accounts-range-value").style.opacity = auto ? "0.4" : "1";
});

// Approximate mainnet chain tip and scan rate for time estimates
const APPROX_CHAIN_TIP = 2_730_000;
const BLOCKS_PER_MINUTE = 38_000;

function updateScanEstimate() {
  const birthday = parseInt($("birthday-height").value, 10) || 419200;
  const blocks = Math.max(0, APPROX_CHAIN_TIP - birthday);
  const minutes = Math.round(blocks / BLOCKS_PER_MINUTE);
  const el = $("birthday-scan-estimate");
  if (minutes <= 1) {
    el.textContent = "Estimated scan time: under 1 minute.";
  } else if (minutes < 60) {
    el.textContent = `Estimated scan time: ~${minutes} minutes.`;
  } else {
    const hours = Math.floor(minutes / 60);
    const mins = minutes % 60;
    el.textContent = `Estimated scan time: ~${hours}h ${mins}m.`;
  }
}

$("birthday-height").addEventListener("input", updateScanEstimate);
updateScanEstimate();

$("birthday-estimate").addEventListener("click", async () => {
  const dateVal = $("birthday-date").value;
  if (!dateVal) {
    setStatus("config-status", "Pick a date first.", "error");
    return;
  }
  try {
    const height = await invoke("estimate_birthday_from_date", { date: dateVal });
    $("birthday-height").value = height;
    updateScanEstimate();
    setStatus("config-status", `Birthday estimated: block ${Number(height).toLocaleString()}`, "success");
  } catch (err) {
    setStatus("config-status", String(err), "error");
  }
});

$("destination-validate").addEventListener("click", async () => {
  const address = $("destination-input").value.trim();
  if (!address) {
    setStatus("config-status", "Enter a destination address first.", "error");
    return;
  }
  try {
    const info = await invoke("validate_address", { address });
    if (!info.destination_ok) {
      setStatus("config-status", "✗ Address must have an Orchard or Sapling receiver.", "error");
    } else {
      const pools = [info.has_orchard && "Orchard", info.has_sapling && "Sapling"]
        .filter(Boolean)
        .join(" + ");
      setStatus("config-status", `✓ Valid Unified Address — receivers: ${pools}`, "success");
    }
  } catch (err) {
    setStatus("config-status", `✗ ${err}`, "error");
  }
});

$("start-scan").addEventListener("click", async () => {
  const address = $("destination-input").value.trim();
  if (!address) {
    setStatus("config-status", "A destination Unified Address is required.", "error");
    return;
  }

  try {
    const info = await invoke("validate_address", { address });
    if (!info.destination_ok) {
      setStatus("config-status", "✗ Address must have an Orchard or Sapling receiver.", "error");
      return;
    }
  } catch (err) {
    setStatus("config-status", `✗ ${err}`, "error");
    return;
  }

  state.destination = address;
  state.memo = $("sweep-memo").value.trim() || null;
  state.maxFeeZec = $("max-fee-zec").value.trim() || null;

  const autoGap = $("auto-gap-limit").checked;
  const config = {
    seed: seedInput.value.trim().toLowerCase(),
    birthday: parseInt($("birthday-height").value, 10) || 419200,
    num_accounts: autoGap ? null : parseInt($("accounts-range").value, 10),
    gap_limit: autoGap ? parseInt($("gap-limit").value, 10) : 20,
    lightwalletd_url: $("lightwalletd-url").value.trim(),
    data_dir: $("data-dir").value.trim() || "./zeck_data",
    network: $("network-select").value,
  };

  setStatus("config-status", "Starting scan…", "");
  $("start-scan").disabled = true;

  try {
    const handle = await invoke("start_scan", { config });
    state.scanHandle = handle;
    goTo("scan");
    await startProgressListeners();
  } catch (err) {
    setStatus("config-status", `✗ ${err}`, "error");
    $("start-scan").disabled = false;
  }
});

// ─── Step 4: Scan Progress ────────────────────────────────────────────────────

async function startProgressListeners() {
  $("scan-phase").textContent = "Starting…";
  $("scan-server").textContent = "Connecting…";
  $("scan-progress-text").textContent = "0 / 0";
  $("scan-eta").textContent = "0s / —";
  $("scan-progress-bar").style.width = "0%";
  $("scan-rows").innerHTML = "";
  setStatus("scan-message", "", "");
  $("review-sweep").disabled = true;
  $("back-to-config").style.display = "none";

  // Await all three subscriptions synchronously. If we stored the unlisten
  // handles via `.then(...)` callbacks, a fast `scan-complete` event could
  // fire and run `cleanupListeners()` before the handles were assigned,
  // leaving the subscriptions alive and leaking across scans.
  const [unlistenProgress, unlistenComplete, unlistenDiscovered] = await Promise.all([
    listen("scan-progress", (event) => updateScanUI(event.payload)),
    listen("scan-complete", (event) => {
      updateScanUI(event.payload);
      cleanupListeners();
    }),
    listen("account-discovered", () => {}),
  ]);
  state.unlistenProgress = unlistenProgress;
  state.unlistenComplete = unlistenComplete;
  state.unlistenDiscovered = unlistenDiscovered;
}

function cleanupListeners() {
  state.unlistenProgress?.();
  state.unlistenComplete?.();
  state.unlistenDiscovered?.();
  state.unlistenProgress = null;
  state.unlistenComplete = null;
  state.unlistenDiscovered = null;
}

function updateScanUI(progress) {
  state.lastProgress = progress;

  $("scan-phase").textContent = phaseLabel(progress.phase);

  if (progress.server?.endpoint) {
    const primary = $("lightwalletd-url").value.split(",")[0].trim();
    const isFallback = progress.server.endpoint !== primary;
    $("scan-server").textContent = progress.server.endpoint + (isFallback ? " (fallback)" : "");
  }

  const scanned = Number(progress.blocks_scanned);
  const total = Number(progress.blocks_total);
  $("scan-progress-text").textContent =
    `${scanned.toLocaleString()} / ${total.toLocaleString()}`;

  if (total > 0) {
    $("scan-progress-bar").style.width =
      `${Math.min(100, (scanned / total) * 100).toFixed(1)}%`;
  }

  $("scan-eta").textContent =
    `${fmtSeconds(progress.elapsed_seconds)} / ${fmtSeconds(progress.estimated_remaining_seconds)}`;

  if (progress.error) {
    setStatus("scan-message", progress.error, "error");
    $("back-to-config").style.display = "";
  } else if (progress.message) {
    setStatus("scan-message", progress.message, "");
  }

  if (progress.summary) {
    const s = progress.summary;
    const acctCount = progress.accounts.length;
    $("scan-totals").textContent =
      `Grand total: ${fmt(s.total_zatoshis)} across ${acctCount} account(s).${s.authoritative_balances ? "" : " (estimates)"}`;
    $("scan-workspace").textContent = `Workspace: ${s.workspace_dir}`;
  }

  renderAccountRows(progress.accounts);

  if (progress.phase === "complete") {
    $("review-sweep").disabled = false;
  }
}

function renderAccountRows(accounts) {
  const tbody = $("scan-rows");
  tbody.innerHTML = "";
  accounts.forEach((acc) => {
    const tr = document.createElement("tr");
    tr.innerHTML = `
      <td>${acc.account_index}</td>
      <td>${fmt(acc.sapling_zatoshis)}</td>
      <td>${fmt(acc.orchard_zatoshis)}</td>
      <td>${fmt(acc.transparent_zatoshis)}</td>
      <td>${fmt(acc.total_zatoshis)}</td>
      <td>${escapeHtml(acc.status)}</td>
    `;
    tbody.appendChild(tr);
  });
}

$("back-to-config").addEventListener("click", () => {
  // Must release event subscriptions before leaving the scan screen —
  // otherwise a subsequent start_scan would stack a second set of listeners
  // on top of the first and double every progress event.
  cleanupListeners();
  state.scanHandle = null;
  $("back-to-config").style.display = "none";
  $("start-scan").disabled = false;
  goTo("config");
});

$("cancel-scan").addEventListener("click", async () => {
  if (!state.scanHandle) return;
  try {
    await invoke("cancel_scan", { handle: state.scanHandle });
    cleanupListeners();
    setStatus("scan-message", "Scan cancelled. Workspace state preserved on disk.", "");
    $("scan-phase").textContent = "Cancelled";
    $("back-to-config").style.display = "";
    $("start-scan").disabled = false;
  } catch (err) {
    setStatus("scan-message", `Cancel failed: ${err}`, "error");
  }
});

$("review-sweep").addEventListener("click", async () => {
  setStatus("scan-message", "Fetching sweep proposal…", "");
  $("review-sweep").disabled = true;

  try {
    const proposal = await invoke("propose_sweep", {
      handle: state.scanHandle,
      destination: state.destination,
      memo: state.memo,
      maxFeeZec: state.maxFeeZec,
    });
    state.sweepProposal = proposal;
    renderSweepProposal(proposal);
    goTo("sweep");
  } catch (err) {
    setStatus("scan-message", `✗ ${err}`, "error");
    $("review-sweep").disabled = false;
  }
});

// ─── Step 5: Sweep Review ─────────────────────────────────────────────────────

function renderSweepProposal(proposal) {
  const tbody = $("sweep-rows");
  tbody.innerHTML = "";

  proposal.transactions.forEach((tx) => {
    const kindLabel = tx.kind === "shield_transparent" ? "Shield" : "Sweep";
    const dest = tx.destination;
    const shortDest =
      dest.length > 26 ? dest.slice(0, 12) + "…" + dest.slice(-10) : dest;
    const tr = document.createElement("tr");
    tr.innerHTML = `
      <td>${tx.source_account}</td>
      <td>${kindLabel}</td>
      <td title="${escapeHtml(dest)}">${escapeHtml(shortDest)}</td>
      <td>${fmt(tx.gross_zatoshis)}</td>
      <td>${fmt(tx.fee_zatoshis)}</td>
      <td>${fmt(tx.net_zatoshis)}</td>
      <td>${escapeHtml(tx.memo ?? "—")}</td>
    `;
    tbody.appendChild(tr);
  });

  $("sweep-summary").textContent =
    `Net received: ${fmt(proposal.net_received_zatoshis)} after ${fmt(proposal.total_fee_zatoshis)} in fees.` +
    (proposal.warning ? `  ⚠ ${proposal.warning}` : "");

  const skippedEl = $("sweep-skipped");
  if (proposal.skipped_accounts.length > 0) {
    const items = proposal.skipped_accounts
      .map(
        (s) =>
          `<li>Account ${s.account_index}: ${escapeHtml(s.reason)} (${fmt(s.gross_zatoshis)})</li>`
      )
      .join("");
    skippedEl.innerHTML = `<p style="margin:6px 0 4px;font-weight:700;color:var(--muted)">Skipped accounts</p><ul class="discovery-list">${items}</ul>`;
  } else {
    skippedEl.innerHTML = "";
  }

  $("irreversible-check").checked = false;
  $("execute-sweep").disabled = true;
}

$("irreversible-check").addEventListener("change", () => {
  $("execute-sweep").disabled = !$("irreversible-check").checked;
});

$("execute-sweep").addEventListener("click", async () => {
  $("execute-sweep").disabled = true;
  $("irreversible-check").disabled = true;

  try {
    const results = await invoke("execute_sweep", {
      handle: state.scanHandle,
      destination: state.destination,
      memo: state.memo,
      maxFeeZec: state.maxFeeZec,
    });
    renderCompleteScreen(results);
    goTo("complete");
  } catch (err) {
    $("execute-sweep").disabled = false;
    $("irreversible-check").disabled = false;
    $("sweep-skipped").innerHTML =
      `<p class="status-line error">✗ Sweep failed: ${escapeHtml(String(err))}</p>`;
  }
});

// ─── Step 6: Complete ─────────────────────────────────────────────────────────

function renderCompleteScreen(results) {
  const confirmed = results.filter((r) => r.status === "confirmed").length;
  const pending = results.filter((r) => r.status === "pending").length;
  const failed = results.filter((r) => r.status === "failed").length;

  $("complete-summary").textContent =
    `Sweep finished — ${confirmed} confirmed, ${pending} pending, ${failed} failed.`;

  const rows = results
    .map((r) => {
      let line = `Account ${r.source_account}: ${r.status.toUpperCase()}`;
      if (r.txid) line += `\n  txid: ${r.txid}`;
      if (r.confirmed_height) line += `\n  confirmed at block ${r.confirmed_height}`;
      if (r.detail) line += `\n  ${r.detail}`;
      return line;
    })
    .join("\n\n");

  $("complete-report").innerHTML = `<pre>${escapeHtml(rows)}</pre>`;

  const report = buildReport(results);
  $("save-report").dataset.report = report;
  $("report-path").value = buildDefaultReportPath();
}

function buildReport(results) {
  return [
    "ZECK Recovery Report",
    `Date: ${new Date().toISOString()}`,
    "",
    "Transaction Results",
    "──────────────────",
    ...results.map((r) => {
      let line = `Account ${r.source_account}: ${r.status}`;
      if (r.txid) line += `\n  txid: ${r.txid}`;
      if (r.confirmed_height) line += `\n  confirmed at block ${r.confirmed_height}`;
      if (r.detail) line += `\n  detail: ${r.detail}`;
      return line;
    }),
  ].join("\n");
}

function buildDefaultReportPath() {
  const dir = ($("data-dir")?.value ?? "").trim();
  if (!dir) return "zeck-recovery-report.txt";
  const sep = dir.includes("\\") && !dir.includes("/") ? "\\" : "/";
  return `${dir}${sep}zeck-recovery-report.txt`;
}

$("save-report").addEventListener("click", async () => {
  const path = $("report-path").value.trim();
  const report = $("save-report").dataset.report ?? "";
  if (!report) {
    setStatus("save-report-status", "Nothing to save yet.", "error");
    return;
  }
  try {
    const saved = await invoke("save_recovery_report", { path, report });
    setStatus("save-report-status", `✓ Saved to ${saved}`, "success");
  } catch (err) {
    setStatus("save-report-status", `✗ ${err}`, "error");
  }
});

$("restart-flow").addEventListener("click", () => {
  cleanupListeners();
  Object.assign(state, {
    scanHandle: null,
    lastProgress: null,
    sweepProposal: null,
    destination: null,
    memo: null,
    maxFeeZec: null,
  });

  seedInput.value = "";
  seedVisibility.checked = false;
  seedInput.classList.add("masked");
  seedNextBtn.disabled = true;
  setStatus("seed-status", "", "");
  setStatus("config-status", "", "");
  $("destination-input").value = "";
  $("max-fee-zec").value = "";
  $("sweep-memo").value = "";
  $("start-scan").disabled = false;

  goTo("welcome");
});

// ─── Init ─────────────────────────────────────────────────────────────────────

$("lightwalletd-url").value = SERVER_PRESETS.mainnet;
$("gap-limit-row").style.display = $("auto-gap-limit").checked ? "none" : "block";
$("accounts-range").disabled = !$("auto-gap-limit").checked;
goTo("welcome");

// Populate the workspace dir with the OS-appropriate path from Tauri
// (macOS app data dir, Linux XDG, Windows %APPDATA%) instead of hard-coding
// `/tmp/zeck_data`, which doesn't exist on Windows.
invoke("default_data_dir")
  .then((dir) => {
    if (dir && !$("data-dir").value.trim()) $("data-dir").value = dir;
  })
  .catch(() => {
    // Non-fatal: user can always type a path manually.
  });

}); // end DOMContentLoaded
