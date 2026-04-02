import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const state = {
  currentStep: "welcome",
  scanHandle: null,
  scanProgress: null,
  destinationInfo: null,
  sweepProposal: null,
};

const stepOrder = ["welcome", "seed", "config", "scan", "sweep", "complete"];

const elements = {
  seedInput: document.querySelector("#seed-input"),
  seedVisibility: document.querySelector("#seed-visibility"),
  seedStatus: document.querySelector("#seed-status"),
  seedNext: document.querySelector("#seed-next"),
  seedValidate: document.querySelector("#seed-validate"),
  birthdayHeight: document.querySelector("#birthday-height"),
  birthdayDate: document.querySelector("#birthday-date"),
  accountsRange: document.querySelector("#accounts-range"),
  accountsRangeValue: document.querySelector("#accounts-range-value"),
  gapLimit: document.querySelector("#gap-limit"),
  lightwalletdUrl: document.querySelector("#lightwalletd-url"),
  destinationInput: document.querySelector("#destination-input"),
  configStatus: document.querySelector("#config-status"),
  startScan: document.querySelector("#start-scan"),
  birthdayEstimate: document.querySelector("#birthday-estimate"),
  destinationValidate: document.querySelector("#destination-validate"),
  cancelScan: document.querySelector("#cancel-scan"),
  reviewSweep: document.querySelector("#review-sweep"),
  scanPhase: document.querySelector("#scan-phase"),
  scanServer: document.querySelector("#scan-server"),
  scanProgressText: document.querySelector("#scan-progress-text"),
  scanProgressBar: document.querySelector("#scan-progress-bar"),
  scanMessage: document.querySelector("#scan-message"),
  scanRows: document.querySelector("#scan-rows"),
  sweepRows: document.querySelector("#sweep-rows"),
  sweepSummary: document.querySelector("#sweep-summary"),
  irreversibleCheck: document.querySelector("#irreversible-check"),
  executeSweep: document.querySelector("#execute-sweep"),
  completeSummary: document.querySelector("#complete-summary"),
  completeReport: document.querySelector("#complete-report"),
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
document.querySelector("#restart-flow").addEventListener("click", resetFlow);

listen("scan-progress", (event) => {
  state.scanProgress = event.payload;
  renderScanProgress();
});

listen("scan-complete", (event) => {
  state.scanProgress = event.payload;
  renderScanProgress();
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
  state.scanHandle = null;
  renderScanProgress();

  try {
    state.scanHandle = await invoke("start_scan", {
      config: {
        seed: normalizedSeedPhrase(),
        birthday: Number(elements.birthdayHeight.value || 419200),
        num_accounts: Number(elements.accountsRange.value),
        gap_limit: Number(elements.gapLimit.value || 20),
        lightwalletd_url: elements.lightwalletdUrl.value.trim(),
        network: "mainnet",
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
  if (!state.scanHandle) {
    return;
  }

  try {
    state.sweepProposal = await invoke("propose_sweep", {
      handle: state.scanHandle,
      destination: elements.destinationInput.value.trim(),
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
    const results = await invoke("execute_sweep", {
      handle: state.scanHandle,
      destination: elements.destinationInput.value.trim(),
    });
    state.completeSummary.textContent = "Sweep request finished.";
    state.completeReport.innerHTML = `<pre>${JSON.stringify(results, null, 2)}</pre>`;
    setStep("complete");
  } catch (error) {
    state.completeSummary.textContent = "Sweep execution is not available in this build.";
    state.completeReport.innerHTML = `<pre>${String(error)}</pre>`;
    setStep("complete");
  }
}

function renderScanProgress() {
  const progress = state.scanProgress;
  if (!progress) {
    elements.scanPhase.textContent = "Idle";
    elements.scanServer.textContent = "Not connected";
    elements.scanProgressText.textContent = "0 / 0";
    elements.scanProgressBar.style.width = "0%";
    elements.scanMessage.textContent = "Waiting to start.";
    elements.scanRows.innerHTML = "";
    return;
  }

  elements.scanPhase.textContent = progress.phase.replaceAll("_", " ");
  elements.scanServer.textContent = progress.server
    ? `${progress.server.vendor || "lightwalletd"} @ ${progress.server.latest_block_height || 0}`
    : "Connecting";
  elements.scanProgressText.textContent = `${progress.blocks_scanned} / ${progress.blocks_total}`;
  const percent =
    progress.blocks_total > 0
      ? Math.min(100, Math.round((progress.blocks_scanned / progress.blocks_total) * 100))
      : 0;
  elements.scanProgressBar.style.width = `${percent}%`;
  elements.scanMessage.textContent = progress.message || "Working";
  elements.scanMessage.className = progress.error ? "status-line error" : "status-line";

  elements.scanRows.innerHTML = progress.accounts
    .map(
      (account) => `
        <tr>
          <td>${account.account_index}</td>
          <td><code>${account.sapling_address}</code></td>
          <td><code>${account.unified_address}</code></td>
          <td><code>${account.transparent_receive_address}</code></td>
          <td>${account.status}</td>
        </tr>
      `,
    )
    .join("");

  if (["complete", "cancelled", "error"].includes(progress.phase)) {
    elements.reviewSweep.disabled = false;
  }
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
              <td><code>${tx.destination}</code></td>
              <td>${tx.gross_zatoshis}</td>
              <td>${tx.fee_zatoshis}</td>
              <td>${tx.net_zatoshis}</td>
            </tr>
          `,
        )
        .join("")
    : `<tr><td colspan="5">No spendable balances were found in the current preview.</td></tr>`;

  elements.sweepSummary.textContent = state.sweepProposal.warning || "Proposal ready.";
  elements.sweepSummary.className = "status-line";
}

function splitSeedWords() {
  return normalizedSeedPhrase()
    .split(/\s+/)
    .filter(Boolean);
}

function normalizedSeedPhrase() {
  return elements.seedInput.value.trim().replace(/\s+/g, " ");
}

function setStep(step) {
  state.currentStep = step;
  document.querySelectorAll(".screen").forEach((screen) => {
    screen.classList.toggle("active", screen.dataset.step === step);
  });
  document.querySelectorAll("[data-step-indicator]").forEach((item) => {
    const active = item.dataset.stepIndicator === step;
    item.classList.toggle("active", active);
  });
}

function resetFlow() {
  state.scanHandle = null;
  state.scanProgress = null;
  state.sweepProposal = null;
  state.destinationInfo = null;
  elements.destinationInput.value = "";
  elements.completeReport.innerHTML = "";
  elements.irreversibleCheck.checked = false;
  elements.executeSweep.disabled = true;
  setStep("welcome");
}
