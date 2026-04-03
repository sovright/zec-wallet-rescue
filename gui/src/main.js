import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const state = {
  currentStep: "welcome",
  scanHandle: null,
  scanProgress: null,
  destinationInfo: null,
  sweepProposal: null,
  sweepResults: [],
  accountDiscoveries: [],
};

const stepOrder = ["welcome", "seed", "config", "scan", "sweep", "complete"];
const KNOWN_SERVERS = {
  mainnet: "https://mainnet.lightwalletd.com:9067",
  testnet: "https://testnet.lightwalletd.com:9067",
};

const elements = {
  seedInput: document.querySelector("#seed-input"),
  seedVisibility: document.querySelector("#seed-visibility"),
  seedStatus: document.querySelector("#seed-status"),
  seedNext: document.querySelector("#seed-next"),
  seedValidate: document.querySelector("#seed-validate"),
  networkSelect: document.querySelector("#network-select"),
  birthdayHeight: document.querySelector("#birthday-height"),
  birthdayDate: document.querySelector("#birthday-date"),
  accountsRange: document.querySelector("#accounts-range"),
  accountsRangeValue: document.querySelector("#accounts-range-value"),
  autoGapLimit: document.querySelector("#auto-gap-limit"),
  gapLimit: document.querySelector("#gap-limit"),
  serverPreset: document.querySelector("#server-preset"),
  lightwalletdUrl: document.querySelector("#lightwalletd-url"),
  dataDir: document.querySelector("#data-dir"),
  destinationInput: document.querySelector("#destination-input"),
  sweepMemo: document.querySelector("#sweep-memo"),
  maxFeeZec: document.querySelector("#max-fee-zec"),
  configStatus: document.querySelector("#config-status"),
  startScan: document.querySelector("#start-scan"),
  birthdayEstimate: document.querySelector("#birthday-estimate"),
  destinationValidate: document.querySelector("#destination-validate"),
  cancelScan: document.querySelector("#cancel-scan"),
  reviewSweep: document.querySelector("#review-sweep"),
  scanPhase: document.querySelector("#scan-phase"),
  scanServer: document.querySelector("#scan-server"),
  scanProgressText: document.querySelector("#scan-progress-text"),
  scanEta: document.querySelector("#scan-eta"),
  scanProgressBar: document.querySelector("#scan-progress-bar"),
  scanMessage: document.querySelector("#scan-message"),
  scanTotals: document.querySelector("#scan-totals"),
  scanWorkspace: document.querySelector("#scan-workspace"),
  scanRows: document.querySelector("#scan-rows"),
  scanDiscoveries: document.querySelector("#scan-discoveries"),
  sweepRows: document.querySelector("#sweep-rows"),
  sweepSummary: document.querySelector("#sweep-summary"),
  sweepSkipped: document.querySelector("#sweep-skipped"),
  irreversibleCheck: document.querySelector("#irreversible-check"),
  executeSweep: document.querySelector("#execute-sweep"),
  completeSummary: document.querySelector("#complete-summary"),
  completeReport: document.querySelector("#complete-report"),
  reportPath: document.querySelector("#report-path"),
  saveReport: document.querySelector("#save-report"),
  saveReportStatus: document.querySelector("#save-report-status"),
};

document.querySelectorAll("[data-next]").forEach((button) => {
  button.addEventListener("click", () => setStep(button.dataset.next));
});

document.querySelectorAll("[data-prev]").forEach((button) => {
  button.addEventListener("click", () => setStep(button.dataset.prev));
});

elements.accountsRange.addEventListener("input", () => {
  elements.accountsRangeValue.textContent = elements.accountsRange.value;
});
elements.autoGapLimit.addEventListener("change", syncAccountMode);
elements.gapLimit.addEventListener("input", syncAccountMode);
elements.networkSelect.addEventListener("change", () => {
  applyServerPreset();
  syncReportPath();
});
elements.serverPreset.addEventListener("change", applyServerPreset);
elements.dataDir.addEventListener("input", syncReportPath);

elements.seedVisibility.addEventListener("change", () => {
  elements.seedInput.classList.toggle("masked", !elements.seedVisibility.checked);
});

elements.seedValidate.addEventListener("click", validateSeed);
elements.destinationValidate.addEventListener("click", validateDestination);
elements.birthdayEstimate.addEventListener("click", estimateBirthday);
elements.startScan.addEventListener("click", startScan);
elements.cancelScan.addEventListener("click", cancelScan);
elements.reviewSweep.addEventListener("click", reviewSweep);
elements.irreversibleCheck.addEventListener("change", () => {
  elements.executeSweep.disabled = !elements.irreversibleCheck.checked;
});
elements.executeSweep.addEventListener("click", executeSweep);
elements.saveReport.addEventListener("click", saveRecoveryReport);
document.querySelector("#restart-flow").addEventListener("click", resetFlow);

listen("scan-progress", (event) => {
  state.scanProgress = event.payload;
  renderScanProgress();
});

listen("account-discovered", (event) => {
  const account = event.payload;
  state.accountDiscoveries = [
    account,
    ...state.accountDiscoveries.filter((entry) => entry.account_index !== account.account_index),
  ].slice(0, 12);
  renderDiscoveries();
});

listen("scan-complete", (event) => {
  state.scanProgress = event.payload;
  renderScanProgress();
});

listen("sweep-tx-broadcast", (event) => {
  mergeSweepResult(event.payload);
  renderCompleteReport();
});

listen("sweep-tx-confirmed", (event) => {
  mergeSweepResult(event.payload);
  renderCompleteReport();
});

async function validateSeed() {
  const words = splitSeedWords();
  if (words.length !== 24) {
    elements.seedStatus.textContent = `Expected 24 words, found ${words.length}.`;
    elements.seedStatus.className = "status-line error";
    elements.seedNext.disabled = true;
    return;
  }

  try {
    await invoke("validate_seed", { words });
    elements.seedStatus.textContent = "Seed phrase checksum looks valid.";
    elements.seedStatus.className = "status-line success";
    elements.seedNext.disabled = false;
  } catch (error) {
    elements.seedStatus.textContent = String(error);
    elements.seedStatus.className = "status-line error";
    elements.seedNext.disabled = true;
  }
}

async function validateDestination() {
  if (!elements.destinationInput.value.trim()) {
    elements.configStatus.textContent = "Enter a destination Unified Address first.";
    elements.configStatus.className = "status-line error";
    return;
  }

  try {
    state.destinationInfo = await invoke("validate_address", {
      address: elements.destinationInput.value.trim(),
    });
    elements.configStatus.textContent = `Destination accepted. Orchard=${state.destinationInfo.has_orchard}, Sapling=${state.destinationInfo.has_sapling}.`;
    elements.configStatus.className = "status-line success";
  } catch (error) {
    elements.configStatus.textContent = String(error);
    elements.configStatus.className = "status-line error";
  }
}

async function estimateBirthday() {
  if (!elements.birthdayDate.value) {
    elements.configStatus.textContent = "Choose a date first.";
    elements.configStatus.className = "status-line error";
    return;
  }

  try {
    const height = await invoke("estimate_birthday_from_date", {
      date: elements.birthdayDate.value,
    });
    elements.birthdayHeight.value = String(height);
    elements.configStatus.textContent = `Estimated birthday height: ${height}`;
    elements.configStatus.className = "status-line success";
  } catch (error) {
    elements.configStatus.textContent = String(error);
    elements.configStatus.className = "status-line error";
  }
}

async function startScan() {
  const words = splitSeedWords();
  if (words.length !== 24) {
    elements.configStatus.textContent = "Validate the 24-word seed phrase first.";
    elements.configStatus.className = "status-line error";
    setStep("seed");
    return;
  }

  setStep("scan");
  elements.reviewSweep.disabled = true;
  state.scanProgress = null;
  state.sweepProposal = null;
  state.sweepResults = [];
  state.accountDiscoveries = [];
  state.scanHandle = null;
  renderScanProgress();
  renderDiscoveries();
  syncReportPath();

  try {
    state.scanHandle = await invoke("start_scan", {
      config: {
        seed: normalizedSeedPhrase(),
        birthday: Number(elements.birthdayHeight.value || 419200),
        num_accounts: elements.autoGapLimit.checked ? null : Number(elements.accountsRange.value),
        gap_limit: Number(elements.gapLimit.value || 20),
        lightwalletd_url: elements.lightwalletdUrl.value.trim(),
        data_dir: elements.dataDir.value.trim() || "./zeck_data",
        network: elements.networkSelect.value,
      },
    });
    elements.seedInput.value = "";
    elements.seedStatus.textContent = "";
  } catch (error) {
    elements.scanMessage.textContent = String(error);
    elements.scanMessage.className = "status-line error";
  }
}

async function cancelScan() {
  if (!state.scanHandle) {
    return;
  }

  await invoke("cancel_scan", { handle: state.scanHandle });
}

async function reviewSweep() {
  if (!state.scanHandle || !isSweepReviewAvailable(state.scanProgress)) {
    return;
  }

  try {
    state.sweepProposal = await invoke("propose_sweep", {
      handle: state.scanHandle,
      destination: elements.destinationInput.value.trim(),
      memo: elements.sweepMemo.value.trim() || null,
      maxFeeZec: elements.maxFeeZec.value.trim() || null,
    });
    renderSweepProposal();
    setStep("sweep");
  } catch (error) {
    elements.scanMessage.textContent = String(error);
    elements.scanMessage.className = "status-line error";
  }
}

async function executeSweep() {
  if (!state.scanHandle) {
    return;
  }

  try {
    state.sweepResults = await invoke("execute_sweep", {
      handle: state.scanHandle,
      destination: elements.destinationInput.value.trim(),
      memo: elements.sweepMemo.value.trim() || null,
      maxFeeZec: elements.maxFeeZec.value.trim() || null,
    });
    elements.completeSummary.textContent = "Sweep request finished.";
    renderCompleteReport();
    syncReportPath();
    setStep("complete");
  } catch (error) {
    state.sweepResults = [];
    elements.completeSummary.textContent = "Sweep execution ended with an error.";
    elements.completeReport.innerHTML = `<pre>${String(error)}</pre>`;
    syncReportPath();
    setStep("complete");
  }
}

async function saveRecoveryReport() {
  try {
    const savedPath = await invoke("save_recovery_report", {
      path: elements.reportPath.value.trim(),
      report: buildRecoveryReport(),
    });
    elements.saveReportStatus.textContent = `Saved report to ${savedPath}`;
    elements.saveReportStatus.className = "status-line success";
  } catch (error) {
    elements.saveReportStatus.textContent = String(error);
    elements.saveReportStatus.className = "status-line error";
  }
}

function renderScanProgress() {
  const progress = state.scanProgress;
  if (!progress) {
    elements.scanPhase.textContent = "Idle";
    elements.scanServer.textContent = "Not connected";
    elements.scanProgressText.textContent = "0 / 0";
    elements.scanEta.textContent = "0s / —";
    elements.scanProgressBar.style.width = "0%";
    elements.scanMessage.textContent = "Waiting to start.";
    elements.scanMessage.className = "status-line";
    elements.scanTotals.textContent = "Grand total: 0.00000000 ZEC across 0 accounts.";
    elements.scanWorkspace.textContent = "Workspace: not initialized";
    elements.scanRows.innerHTML = "";
    elements.reviewSweep.disabled = true;
    return;
  }

  elements.scanPhase.textContent = progress.phase.replaceAll("_", " ");
  elements.scanServer.textContent = progress.server
    ? `${progress.server.vendor || "lightwalletd"} @ ${progress.server.latest_block_height || 0}`
    : "Connecting";
  elements.scanProgressText.textContent = `${progress.blocks_scanned} / ${progress.blocks_total}`;
  elements.scanEta.textContent = `${formatDuration(progress.elapsed_seconds)} / ${formatDuration(progress.estimated_remaining_seconds)}`;
  const percent =
    progress.blocks_total > 0
      ? Math.min(100, Math.round((progress.blocks_scanned / progress.blocks_total) * 100))
      : 0;
  elements.scanProgressBar.style.width = `${percent}%`;
  elements.scanMessage.textContent =
    (progress.summary && progress.summary.note) || progress.message || "Working";
  elements.scanMessage.className = progress.error ? "status-line error" : "status-line";

  elements.scanRows.innerHTML = progress.accounts
    .map(
      (account) => `
        <tr>
          <td>${account.account_index}</td>
          <td title="${account.sapling_address}">${formatZec(account.sapling_zatoshis)}</td>
          <td title="${account.unified_address}">${formatZec(account.orchard_zatoshis)}</td>
          <td title="${account.transparent_receive_address}">${formatZec(account.transparent_zatoshis)}</td>
          <td>${formatZec(account.total_zatoshis)}</td>
          <td>${account.status}</td>
        </tr>
      `,
    )
    .join("");

  const fundedAccounts = progress.accounts.filter((account) => account.total_zatoshis > 0).length;
  const totalZatoshis =
    (progress.summary && progress.summary.total_zatoshis) ||
    progress.accounts.reduce((sum, account) => sum + account.total_zatoshis, 0);
  elements.scanTotals.textContent = `Grand total: ${formatZec(totalZatoshis)} across ${fundedAccounts} funded account${fundedAccounts === 1 ? "" : "s"}.`;
  elements.scanWorkspace.textContent = progress.summary
    ? `Workspace: ${progress.summary.workspace_dir}`
    : "Workspace: preparing persisted wallet state";
  elements.reviewSweep.disabled = !isSweepReviewAvailable(progress);
  syncReportPath();
}

function renderSweepProposal() {
  if (!state.sweepProposal) {
    return;
  }

  elements.sweepRows.innerHTML = state.sweepProposal.transactions.length
    ? state.sweepProposal.transactions
        .map(
          (tx) => `
            <tr>
              <td>${tx.source_account}</td>
              <td>${humanizeSweepKind(tx.kind)}</td>
              <td><code>${tx.destination}</code></td>
              <td>${tx.gross_zatoshis}</td>
              <td>${tx.fee_zatoshis}</td>
              <td>${tx.net_zatoshis}</td>
              <td>${tx.memo || "—"}</td>
            </tr>
          `,
        )
        .join("")
    : `<tr><td colspan="7">No spendable balances were found in the completed scan.</td></tr>`;

  elements.sweepSummary.textContent = state.sweepProposal.warning || "Proposal ready.";
  elements.sweepSummary.className = "status-line";
  elements.sweepSkipped.innerHTML = state.sweepProposal.skipped_accounts.length
    ? `<pre>${state.sweepProposal.skipped_accounts
        .map(
          (skipped) =>
            `Account ${skipped.account_index}: ${skipped.gross_zatoshis} zats skipped. ${skipped.reason}`,
        )
        .join("\n")}</pre>`
    : "";
}

function renderCompleteReport() {
  if (!state.sweepResults.length) {
    if (!elements.completeReport.innerHTML.trim()) {
      elements.completeReport.innerHTML =
        "<p>No broadcast results are available for this session yet.</p>";
    }
    return;
  }

  elements.completeReport.innerHTML = `
    <table>
      <thead>
        <tr>
          <th>Account</th>
          <th>Status</th>
          <th>Txid</th>
          <th>Height</th>
          <th>Detail</th>
        </tr>
      </thead>
      <tbody>
        ${state.sweepResults
          .map((result) => {
            const explorerUrl = buildExplorerUrl(result.txid);
            const txidCell = result.txid
              ? explorerUrl
                ? `<a href="${explorerUrl}" target="_blank" rel="noreferrer"><code>${result.txid}</code></a>`
                : `<code>${result.txid}</code>`
              : "—";
            return `
              <tr>
                <td>${result.source_account}</td>
                <td>${result.status}</td>
                <td>${txidCell}</td>
                <td>${result.confirmed_height || "—"}</td>
                <td>${result.detail}</td>
              </tr>
            `;
          })
          .join("")}
      </tbody>
    </table>
  `;
}

function renderDiscoveries() {
  if (!state.accountDiscoveries.length) {
    elements.scanDiscoveries.innerHTML =
      "<p>No funded legacy accounts have been discovered yet.</p>";
    return;
  }

  elements.scanDiscoveries.innerHTML = `
    <strong>Discovery log</strong>
    <ul class="discovery-list">
      ${state.accountDiscoveries
        .map(
          (account) =>
            `<li>Account ${account.account_index}: ${formatZec(account.total_zatoshis)} found (${account.status})</li>`,
        )
        .join("")}
    </ul>
  `;
}

function splitSeedWords() {
  return normalizedSeedPhrase()
    .split(/\s+/)
    .filter(Boolean);
}

function normalizedSeedPhrase() {
  return elements.seedInput.value.trim().replace(/\s+/g, " ");
}

function isSweepReviewAvailable(progress) {
  return progress && progress.phase === "complete" && !progress.error;
}

function humanizeSweepKind(kind) {
  switch (kind) {
    case "shield_transparent":
      return "Shield";
    case "sweep_shielded":
      return "Sweep";
    default:
      return kind;
  }
}

function syncAccountMode() {
  const manualMode = !elements.autoGapLimit.checked;
  elements.accountsRange.disabled = !manualMode;
  elements.accountsRangeValue.textContent = manualMode
    ? elements.accountsRange.value
    : `Auto (${elements.gapLimit.value || 20} gap)`;
}

function applyServerPreset() {
  const preset = elements.serverPreset.value;
  if (preset === "custom") {
    return;
  }

  if (preset === "recommended") {
    elements.lightwalletdUrl.value =
      KNOWN_SERVERS[elements.networkSelect.value] || KNOWN_SERVERS.mainnet;
    return;
  }

  elements.lightwalletdUrl.value =
    KNOWN_SERVERS[preset] || elements.lightwalletdUrl.value;
}

function syncReportPath() {
  const workspaceDir =
    state.scanProgress && state.scanProgress.summary
      ? state.scanProgress.summary.workspace_dir
      : elements.dataDir.value.trim() || "./zeck_data";
  const separator = workspaceDir.endsWith("/") ? "" : "/";
  elements.reportPath.value = `${workspaceDir}${separator}zeck-recovery-report.txt`;
}

function mergeSweepResult(result) {
  const index = state.sweepResults.findIndex((entry) => {
    if (entry.txid && result.txid) {
      return entry.txid === result.txid;
    }
    return entry.source_account === result.source_account && entry.status === result.status;
  });

  if (index >= 0) {
    state.sweepResults[index] = result;
    return;
  }

  state.sweepResults.push(result);
}

function buildRecoveryReport() {
  const lines = [
    "ZECK recovery report",
    "",
    `Phase: ${state.scanProgress ? state.scanProgress.phase : "unknown"}`,
    `Network: ${elements.networkSelect.value}`,
    `Server: ${
      state.scanProgress && state.scanProgress.server
        ? state.scanProgress.server.endpoint
        : elements.lightwalletdUrl.value.trim()
    }`,
    `Workspace: ${
      state.scanProgress && state.scanProgress.summary
        ? state.scanProgress.summary.workspace_dir
        : elements.dataDir.value.trim() || "./zeck_data"
    }`,
    `Total discovered: ${
      state.scanProgress && state.scanProgress.summary
        ? state.scanProgress.summary.total_zatoshis
        : 0
    } zats`,
    "",
    "Accounts:",
  ];

  if (state.scanProgress && state.scanProgress.accounts.length) {
    state.scanProgress.accounts.forEach((account) => {
      lines.push(
        `- Account ${account.account_index}: total=${account.total_zatoshis} sapling=${account.sapling_zatoshis} orchard=${account.orchard_zatoshis} transparent=${account.transparent_zatoshis} status=${account.status}`,
      );
    });
  } else {
    lines.push("- No account rows available");
  }

  lines.push("", "Sweep proposal:");
  if (state.sweepProposal) {
    lines.push(
      `- total_send=${state.sweepProposal.total_send_zatoshis} total_fee=${state.sweepProposal.total_fee_zatoshis} net_received=${state.sweepProposal.net_received_zatoshis}`,
    );
    state.sweepProposal.transactions.forEach((tx) => {
      lines.push(
        `- account=${tx.source_account} kind=${tx.kind} gross=${tx.gross_zatoshis} fee=${tx.fee_zatoshis} net=${tx.net_zatoshis} destination=${tx.destination} memo=${tx.memo || ""}`,
      );
    });
  } else {
    lines.push("- No sweep proposal generated");
  }

  lines.push("", "Broadcast results:");
  if (state.sweepResults.length) {
    state.sweepResults.forEach((result) => {
      lines.push(
        `- account=${result.source_account} status=${result.status} txid=${result.txid || ""} confirmed_height=${result.confirmed_height || ""} detail=${result.detail}`,
      );
    });
  } else {
    lines.push("- No broadcast results recorded");
  }

  return `${lines.join("\n")}\n`;
}

function buildExplorerUrl(txid) {
  if (!txid || elements.networkSelect.value !== "mainnet") {
    return null;
  }
  return `https://blockchair.com/zcash/transaction/${txid}`;
}

function formatZec(zatoshis) {
  return `${(Number(zatoshis || 0) / 100000000).toFixed(8)} ZEC`;
}

function formatDuration(seconds) {
  if (seconds === null || seconds === undefined) {
    return "—";
  }
  const wholeSeconds = Math.max(0, Number(seconds));
  const minutes = Math.floor(wholeSeconds / 60);
  const remainingSeconds = wholeSeconds % 60;
  if (minutes === 0) {
    return `${remainingSeconds}s`;
  }
  return `${minutes}m ${String(remainingSeconds).padStart(2, "0")}s`;
}

function setStep(step) {
  state.currentStep = step;
  const currentIndex = stepOrder.indexOf(step);
  document.querySelectorAll(".screen").forEach((screen) => {
    screen.classList.toggle("active", screen.dataset.step === step);
  });
  document.querySelectorAll("[data-step-indicator]").forEach((item, index) => {
    const active = item.dataset.stepIndicator === step;
    item.classList.toggle("active", active);
    item.classList.toggle("complete", index < currentIndex);
  });
}

function resetFlow() {
  state.scanHandle = null;
  state.scanProgress = null;
  state.sweepProposal = null;
  state.sweepResults = [];
  state.destinationInfo = null;
  state.accountDiscoveries = [];
  elements.destinationInput.value = "";
  elements.sweepMemo.value = "";
  elements.maxFeeZec.value = "";
  elements.completeSummary.textContent = "ZECK finished the current recovery workflow.";
  elements.completeReport.innerHTML = "";
  elements.saveReportStatus.textContent = "";
  elements.saveReportStatus.className = "status-line";
  elements.irreversibleCheck.checked = false;
  elements.executeSweep.disabled = true;
  elements.reviewSweep.disabled = true;
  elements.sweepSkipped.innerHTML = "";
  renderDiscoveries();
  syncReportPath();
  setStep("welcome");
}

syncAccountMode();
applyServerPreset();
renderDiscoveries();
syncReportPath();
