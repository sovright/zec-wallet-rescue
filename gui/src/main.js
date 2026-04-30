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

// Friendly, deliberately imprecise ETA banding. Mirrors `format_eta_range` in
// zeck-cli; if you change one, change both.
function formatEtaRange(secs) {
  if (secs == null || !Number.isFinite(secs) || secs < 0) return null;
  if (secs < 60) return "less than a minute remaining";
  if (secs < 5 * 60) return "less than 5 minutes remaining";
  if (secs < 30 * 60) {
    const mins = Math.round(secs / 60 / 5) * 5;
    return `about ${mins} minutes remaining`;
  }
  if (secs < 60 * 60) return "less than an hour remaining";
  const hours = secs / 3600;
  if (hours < 2) return "about 1-2 hours remaining";
  const lo = Math.floor(hours);
  return `about ${lo}-${lo + 1} hours remaining`;
}

// Map a block height to its approximate calendar year on mainnet so users can
// feel the scan moving through time. Mirrors `era_hint` in zeck-cli.
function eraHint(height) {
  if (!height) return null;
  const SAPLING_HEIGHT = 419_200;
  const SAPLING_YEAR = 2018;
  const SECONDS_PER_BLOCK = 82;
  if (height < SAPLING_HEIGHT) return "pre-Sapling era";
  const elapsedSecs = (height - SAPLING_HEIGHT) * SECONDS_PER_BLOCK;
  const elapsedYears = elapsedSecs / (365.25 * 86400);
  return String(SAPLING_YEAR + Math.floor(elapsedYears + 0.18));
}

// Sliding-window ETA tracker — see `EtaTracker` in zeck-cli.
const eta = (() => {
  const WINDOW_MS = 45_000;
  let samples = [];
  let lastTotal = 0;
  let startedAt = null;
  let lastRate = null; // blocks/sec — reused mid-batch when scannedInWindow=0

  return {
    reset() {
      samples = [];
      lastTotal = 0;
      startedAt = performance.now();
      lastRate = null;
    },
    observe(scanned, total) {
      if (!total) return;
      lastTotal = total;
      const now = performance.now();
      samples.push([now, scanned]);
      const cutoff = now - WINDOW_MS;
      while (samples.length > 2 && samples[0][0] < cutoff) samples.shift();
    },
    estimate() {
      if (startedAt == null || samples.length < 2 || !lastTotal) return { kind: "warmup" };
      const [tLast, blocksLast] = samples[samples.length - 1];
      const remaining = lastTotal - blocksLast;
      if (remaining <= 0) return { kind: "done" };
      const [tFirst, blocksFirst] = samples[0];
      const windowMs = tLast - tFirst;
      const scannedInWindow = blocksLast - blocksFirst;
      // Only update the rate when blocks actually moved within the window.
      // zcash_client_sqlite commits in ~1000-block batches so scannedInWindow
      // is 0 between commits — reuse lastRate so the ETA stays visible mid-batch.
      // Never use startedAt as the origin: on a resume scan blocks_scanned
      // starts large, which would make the rate look astronomically high.
      if (windowMs >= 500 && scannedInWindow >= 1) {
        lastRate = scannedInWindow / (windowMs / 1000);
      }
      if (!lastRate) return { kind: "warmup" };
      const secs = Math.round(remaining / lastRate);
      return { kind: "range", text: formatEtaRange(secs) };
    },
  };
})();

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

async function validateSeed() {
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
}

$("seed-validate").addEventListener("click", validateSeed);
seedInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); validateSeed(); }
});

// ─── Step 3: Configuration ────────────────────────────────────────────────────

const SERVER_PRESETS = {
  mainnet: "https://zec.rocks:443,https://na.zec.rocks:443",
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

$("sweep-memo").addEventListener("input", () => {
  const bytes = new TextEncoder().encode($("sweep-memo").value).length;
  const counter = $("memo-byte-count");
  counter.textContent = `${bytes} / 512 bytes`;
  counter.style.color = bytes > 512 ? "var(--color-error, #c0392b)" : "";
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

$("birthday-autodetect").addEventListener("click", async () => {
  const seedVal = seedInput.value.trim();
  if (!seedVal) {
    setStatus("config-status", "Enter your seed phrase on step 2 first.", "error");
    return;
  }
  $("birthday-autodetect").disabled = true;
  $("birthday-probe-status").textContent = "Starting detection…";
  setStatus("config-status", "", "");

  const unlistenProbe = await listen("birthday-probe-progress", (event) => {
    $("birthday-probe-status").textContent = event.payload;
  });

  try {
    const result = await invoke("detect_birthday", {
      seed: seedVal.toLowerCase(),
      lightwalletdUrl: $("lightwalletd-url").value.trim(),
      network: $("network-select").value,
    });
    $("birthday-height").value = result.birthday;
    updateScanEstimate();
    $("birthday-probe-status").textContent = "";
    setStatus("config-status", `✓ ${result.message}`, "success");
  } catch (err) {
    $("birthday-probe-status").textContent = "";
    setStatus("config-status", `✗ Birthday detection failed: ${err}`, "error");
  } finally {
    $("birthday-autodetect").disabled = false;
    unlistenProbe();
  }
});

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

async function validateDestination() {
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
}

$("destination-validate").addEventListener("click", validateDestination);
$("destination-input").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); validateDestination(); }
});

$("start-scan").addEventListener("click", async () => {
  if (!seedInput.value.trim()) {
    setStatus("config-status", "Seed phrase is required — go back and enter it.", "error");
    return;
  }

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
  const maxFeeRaw = $("max-fee-zec").value.trim();
  if (maxFeeRaw && !/^\d*\.?\d{0,8}$/.test(maxFeeRaw)) {
    setStatus("config-status", "✗ Max fee must be a valid ZEC amount (e.g. 0.0002)", "error");
    $("start-scan").disabled = false;
    return;
  }
  state.maxFeeZec = maxFeeRaw || null;

  let dataDirVal = $("data-dir").value.trim();
  if (!dataDirVal) {
    try {
      dataDirVal = await invoke("default_data_dir");
      $("data-dir").value = dataDirVal;
    } catch (_) {
      setStatus("config-status", "✗ Could not determine a data directory. Please enter one manually.", "error");
      $("start-scan").disabled = false;
      return;
    }
  }

  const autoGap = $("auto-gap-limit").checked;
  const config = {
    seed: seedInput.value.trim().toLowerCase(),
    birthday: parseInt($("birthday-height").value, 10) || 419200,
    num_accounts: autoGap ? null : parseInt($("accounts-range").value, 10),
    gap_limit: autoGap ? parseInt($("gap-limit").value, 10) : 20,
    lightwalletd_url: $("lightwalletd-url").value.trim(),
    data_dir: dataDirVal,
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
  } finally {
    // Clear the seed phrase from the DOM regardless of whether start_scan
    // succeeded; on failure the user can retype, and a successful scan no
    // longer needs the cleartext phrase visible.
    seedInput.value = "";
  }
});

// ─── Step 4: Scan Progress ────────────────────────────────────────────────────

async function startProgressListeners() {
  $("scan-phase").textContent = "Starting…";
  $("scan-server").textContent = "Connecting…";
  $("scan-progress-text").textContent = "0 / 0";
  $("scan-eta").textContent = "Calculating…";
  $("scan-progress-bar").style.width = "0%";
  $("scan-rows").innerHTML = "";
  setStatus("scan-message", "", "");
  $("review-sweep").disabled = true;
  $("back-to-config").style.display = "none";
  eta.reset();

  // Await all three subscriptions before returning. If we stored the unlisten
  // handles via .then() callbacks, a fast scan-complete event could fire and
  // run cleanupListeners() before the handles were assigned, leaking the
  // subscriptions across scans.
  $("scan-discoveries").innerHTML = "";
  $("scan-discoveries").style.display = "none";

  const [unlistenProgress, unlistenComplete, unlistenDiscovered] = await Promise.all([
    listen("scan-progress", (event) => {
      if (event.payload.handle?.id !== state.scanHandle?.id) return;
      updateScanUI(event.payload);
    }),
    listen("scan-complete", (event) => {
      if (event.payload.handle?.id !== state.scanHandle?.id) return;
      updateScanUI(event.payload);
      notifyScanComplete(event.payload);
      cleanupListeners();
    }),
    listen("scan-discovery", (event) => {
      const d = event.payload;
      const div = document.createElement("div");
      div.className = "discovery-toast";
      // at_block_height is the scan frontier when first observed, not the
      // mined height of the funding transaction — label it that way.
      const heightHint = d.at_block_height
        ? ` (scanned through block ${d.at_block_height.toLocaleString()})`
        : "";
      div.textContent =
        `Found ${fmt(d.zatoshis)} on account ${d.account_index} — ${d.pool}${heightHint}. Shielded scan still running — Review & Sweep will unlock when complete.`;
      const container = $("scan-discoveries");
      container.appendChild(div);
      container.style.display = "";
    }),
  ]);
  state.unlistenProgress = unlistenProgress;
  state.unlistenComplete = unlistenComplete;
  state.unlistenDiscovered = unlistenDiscovered;
}

function scanCompletionSummary(progress) {
  if (progress.error) return progress.error;
  // Reserve "no funds were found" for actually-completed scans. A
  // cancelled scan that hadn't yet observed any funds shouldn't claim
  // the seed is empty — it just stopped early.
  if (progress.phase === "cancelled") {
    return "Scan stopped before completion. Re-run with the same flags to resume.";
  }
  const funded = (progress.accounts || []).filter((a) => Number(a.total_zatoshis) > 0);
  if (funded.length === 0) return "No funds were found across all scanned accounts.";
  const total = funded.reduce((sum, a) => sum + Number(a.total_zatoshis), 0);
  const noun = funded.length === 1 ? "account" : "accounts";
  return `Found ${fmt(total)} ${funded.length === 1 ? "on 1" : `across ${funded.length}`} ${noun}.`;
}

function notifyScanComplete(progress) {
  let title;
  switch (progress.phase) {
    case "complete":  title = "ZECK scan complete"; break;
    case "cancelled": title = "ZECK scan cancelled"; break;
    case "error":     title = "ZECK scan failed"; break;
    default: return;
  }
  invoke("notify_user", { title, body: scanCompletionSummary(progress) }).catch(() => {});
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
    $("scan-server").title = isFallback
      ? "Connected to a fallback server — a different operator can see your scan activity"
      : "";
  }

  const scanned = Number(progress.blocks_scanned);
  const total = Number(progress.blocks_total);
  $("scan-progress-text").textContent =
    `${scanned.toLocaleString()} / ${total.toLocaleString()}`;

  if (total > 0) {
    $("scan-progress-bar").style.width =
      `${Math.min(100, (scanned / total) * 100).toFixed(1)}%`;
  }

  eta.observe(scanned, total);
  // eraHint expects an absolute Zcash chain height. blocks_scanned is a
  // delta from effective_birthday — passing it directly mislabels the era
  // for any wallet whose birthday is past Sapling activation. Use
  // synced_to_height (set by the backend) when available.
  const era = progress.synced_to_height ? eraHint(Number(progress.synced_to_height)) : null;
  const etaState = eta.estimate();
  let etaText;
  if (etaState.kind === "warmup") {
    etaText = "Calculating…";
  } else if (etaState.kind === "done") {
    etaText = "";
  } else {
    etaText = etaState.text;
  }
  if (era) etaText = etaText ? `${etaText} · scanning ~${era}` : `scanning ~${era}`;
  $("scan-eta").textContent = etaText;

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

  const terminal = ["complete", "cancelled", "error"].includes(progress.phase);
  $("cancel-scan").style.display = terminal ? "none" : "";
  if (progress.phase === "complete") {
    $("review-sweep").disabled = false;
  }
}

function renderAccountRows(accounts) {
  const tbody = $("scan-rows");
  tbody.replaceChildren();
  accounts.forEach((acc) => {
    const tr = document.createElement("tr");
    appendCell(tr, String(acc.account_index));
    appendCell(tr, fmt(acc.sapling_zatoshis));
    appendCell(tr, fmt(acc.orchard_zatoshis));
    appendCell(tr, fmt(acc.transparent_zatoshis));
    appendCell(tr, fmt(acc.total_zatoshis));
    appendCell(tr, String(acc.status));
    tbody.appendChild(tr);
  });
}

function appendCell(tr, text) {
  const td = document.createElement("td");
  td.textContent = text;
  tr.appendChild(td);
}

$("back-to-config").addEventListener("click", () => {
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
  tbody.replaceChildren();

  proposal.transactions.forEach((tx) => {
    const kindLabel = tx.kind === "shield_transparent" ? "Shield" : "Sweep";
    const dest = tx.destination;
    const shortDest =
      dest.length > 26 ? dest.slice(0, 12) + "…" + dest.slice(-10) : dest;
    const tr = document.createElement("tr");
    appendCell(tr, String(tx.source_account));
    appendCell(tr, kindLabel);

    const destCell = document.createElement("td");
    destCell.title = dest;
    destCell.style.cursor = "pointer";
    destCell.dataset.copy = dest;
    destCell.appendChild(document.createTextNode(shortDest + " "));
    const clip = document.createElement("small");
    clip.textContent = "📋";
    destCell.appendChild(clip);
    tr.appendChild(destCell);

    appendCell(tr, fmt(tx.gross_zatoshis));
    appendCell(tr, fmt(tx.fee_zatoshis));
    appendCell(tr, fmt(tx.net_zatoshis));
    appendCell(tr, String(tx.memo ?? "—"));
    tbody.appendChild(tr);
  });

  $("sweep-summary").textContent =
    `Net received: ${fmt(proposal.net_received_zatoshis)} after ${fmt(proposal.total_fee_zatoshis)} in fees.` +
    (proposal.warning ? `  ⚠ ${proposal.warning}` : "");

  const skippedEl = $("sweep-skipped");
  skippedEl.replaceChildren();
  if (proposal.skipped_accounts.length > 0) {
    const heading = document.createElement("p");
    heading.style.margin = "6px 0 4px";
    heading.style.fontWeight = "700";
    heading.style.color = "var(--muted)";
    heading.textContent = "Skipped accounts";
    skippedEl.appendChild(heading);

    const list = document.createElement("ul");
    list.className = "discovery-list";
    proposal.skipped_accounts.forEach((s) => {
      const li = document.createElement("li");
      li.textContent = `Account ${s.account_index}: ${s.reason} (${fmt(s.gross_zatoshis)})`;
      list.appendChild(li);
    });
    skippedEl.appendChild(list);
  }

  $("irreversible-check").checked = false;
  $("execute-sweep").disabled = true;
}

// Copy-address click handler for sweep table — wired once here so it doesn't
// accumulate duplicates if renderSweepProposal is called more than once.
$("sweep-rows").addEventListener("click", (e) => {
  const cell = e.target.closest("[data-copy]");
  if (!cell) return;
  navigator.clipboard.writeText(cell.dataset.copy).then(() => {
    const orig = cell.cloneNode(true);
    cell.replaceChildren(document.createTextNode("Copied!"));
    setTimeout(() => {
      cell.replaceChildren(...orig.childNodes);
    }, 1200);
  });
});

$("irreversible-check").addEventListener("change", () => {
  $("execute-sweep").disabled = !$("irreversible-check").checked;
});

$("execute-sweep").addEventListener("click", async () => {
  $("execute-sweep").disabled = true;
  $("irreversible-check").disabled = true;
  setStatus("sweep-execute-status", "Broadcasting transactions to the Zcash network… this may take 10–30 seconds.", "");

  try {
    const results = await invoke("execute_sweep", {
      handle: state.scanHandle,
      destination: state.destination,
      memo: state.memo,
      maxFeeZec: state.maxFeeZec,
    });
    setStatus("sweep-execute-status", "", "");
    renderCompleteScreen(results);
    goTo("complete");
  } catch (err) {
    $("execute-sweep").disabled = false;
    $("irreversible-check").disabled = false;
    setStatus("sweep-execute-status", `✗ Sweep failed: ${err}`, "error");
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

  const reportBlock = document.createElement("pre");
  reportBlock.textContent = rows;
  $("complete-report").replaceChildren(reportBlock);

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
  // Paths are resolved relative to the recovery workspace by the backend's
  // resolve_report_path, so we just return the bare file name here.
  return "zeck-recovery-report.txt";
}

$("save-report").addEventListener("click", async () => {
  const path = $("report-path").value.trim();
  const report = $("save-report").dataset.report ?? "";
  if (!report) {
    setStatus("save-report-status", "Nothing to save yet.", "error");
    return;
  }
  try {
    const saved = await invoke("save_recovery_report", {
      handle: state.scanHandle,
      path,
      report,
    });
    setStatus("save-report-status", `✓ Saved to ${saved}`, "success");
  } catch (err) {
    setStatus("save-report-status", `✗ ${err}`, "error");
  }
});

$("restart-flow").addEventListener("click", () => {
  cleanupListeners();
  furthestStep = 0;
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

  // Reset scan screen to blank state so stale results aren't visible if the
  // user navigates forward via the sidebar before starting a new scan.
  $("scan-phase").textContent = "Idle";
  $("scan-server").textContent = "Not connected";
  $("scan-progress-text").textContent = "0 / 0";
  $("scan-eta").textContent = "Calculating…";
  $("scan-progress-bar").style.width = "0%";
  $("scan-rows").innerHTML = "";
  $("scan-discoveries").innerHTML = "";
  $("scan-discoveries").style.display = "none";
  $("scan-totals").textContent = "Grand total: 0.00000000 ZEC across 0 accounts.";
  $("scan-workspace").textContent = "Workspace: not initialized";
  setStatus("scan-message", "", "");
  $("review-sweep").disabled = true;
  $("back-to-config").style.display = "none";
  $("cancel-scan").style.display = "";

  goTo("welcome");
});

// ─── User Guide ───────────────────────────────────────────────────────────────

$("open-guide").addEventListener("click", () => {
  $("guide-overlay").style.display = "";
  document.body.style.overflow = "hidden";
  $("close-guide").focus();
});

$("close-guide").addEventListener("click", () => {
  $("guide-overlay").style.display = "none";
  document.body.style.overflow = "";
});

$("guide-overlay").addEventListener("click", (e) => {
  if (e.target === $("guide-overlay")) {
    $("guide-overlay").style.display = "none";
    document.body.style.overflow = "";
  }
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && $("guide-overlay").style.display !== "none") {
    $("guide-overlay").style.display = "none";
    document.body.style.overflow = "";
  }
});

// ─── Sidebar resize ───────────────────────────────────────────────────────────

const SIDEBAR_MIN = 160;
const SIDEBAR_MAX = 420;
const SIDEBAR_KEY = "zeck-sidebar-w";

(function initSidebarResize() {
  const handle = $("sidebar-resize-handle");
  const shell = document.querySelector(".app-shell");
  const saved = parseInt(localStorage.getItem(SIDEBAR_KEY), 10);
  if (saved >= SIDEBAR_MIN && saved <= SIDEBAR_MAX) {
    shell.style.setProperty("--sidebar-w", saved + "px");
  }

  let dragging = false;
  let startX = 0;
  let startW = 0;

  handle.addEventListener("mousedown", (e) => {
    dragging = true;
    startX = e.clientX;
    startW = parseInt(getComputedStyle(shell).getPropertyValue("--sidebar-w")) || 220;
    handle.classList.add("dragging");
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
  });

  document.addEventListener("mousemove", (e) => {
    if (!dragging) return;
    const w = Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, startW + (e.clientX - startX)));
    shell.style.setProperty("--sidebar-w", w + "px");
  });

  document.addEventListener("mouseup", () => {
    if (!dragging) return;
    dragging = false;
    handle.classList.remove("dragging");
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
    const w = parseInt(getComputedStyle(shell).getPropertyValue("--sidebar-w")) || 220;
    localStorage.setItem(SIDEBAR_KEY, w);
  });
})();

// ─── Init ─────────────────────────────────────────────────────────────────────

$("lightwalletd-url").value = SERVER_PRESETS.mainnet;
$("gap-limit-row").style.display = $("auto-gap-limit").checked ? "none" : "block";
$("accounts-range").disabled = !$("auto-gap-limit").checked;
goTo("welcome");

invoke("default_data_dir")
  .then((dir) => {
    if (dir && !$("data-dir").value.trim()) $("data-dir").value = dir;
  })
  .catch(() => {
    // Non-fatal: user can always type a path manually.
  });

}); // end DOMContentLoaded
