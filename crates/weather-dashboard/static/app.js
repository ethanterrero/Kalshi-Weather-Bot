const tradesBody = document.getElementById("tradesBody");
const rowTemplate = document.getElementById("rowTemplate");
const refreshBtn = document.getElementById("refreshBtn");
const searchInput = document.getElementById("searchInput");
const statusFilter = document.getElementById("statusFilter");

const madeCount = document.getElementById("madeCount");
const heldCount = document.getElementById("heldCount");
const closedCount = document.getElementById("closedCount");
const watchCount = document.getElementById("watchCount");

async function loadDashboard() {
  const params = new URLSearchParams();
  const q = searchInput.value.trim();
  if (q) params.set("q", q);
  if (statusFilter.value) params.set("status", statusFilter.value);
  const response = await fetch(`/api/trades?${params.toString()}`);
  if (!response.ok) {
    throw new Error("Failed to load dashboard");
  }
  return response.json();
}

function setSummary(summary) {
  madeCount.textContent = summary.made;
  heldCount.textContent = summary.held;
  closedCount.textContent = summary.closed;
  watchCount.textContent = summary.watch;
}

function fmtDecimal(v) {
  if (v === null || v === undefined) return "-";
  return Number(v).toFixed(3);
}

function buildRow(trade) {
  const fragment = rowTemplate.content.cloneNode(true);
  const row = fragment.querySelector("tr");
  row.dataset.id = trade.id;

  row.querySelector(".ticker").textContent = trade.ticker;
  row.querySelector(".side").textContent = trade.side;
  row.querySelector(".resolution").textContent = trade.resolution_date;
  row.querySelector(".contracts").textContent = trade.contracts;
  row.querySelector(".limit").textContent = fmtDecimal(trade.limit_price);
  row.querySelector(".edge").textContent = fmtDecimal(trade.raw_edge);
  row.querySelector(".execution").textContent = trade.execution_outcome ?? "-";

  const statusSelect = row.querySelector(".statusSelect");
  statusSelect.value = trade.status;
  statusSelect.classList.add(`status-${trade.status}`);
  statusSelect.addEventListener("change", () => {
    statusSelect.classList.remove("status-held", "status-closed", "status-watch");
    statusSelect.classList.add(`status-${statusSelect.value}`);
  });

  row.querySelector(".sourceChip").textContent = trade.status_source;
  const notesInput = row.querySelector(".notesInput");
  notesInput.value = trade.notes ?? "";

  const saveBtn = row.querySelector(".saveBtn");
  saveBtn.addEventListener("click", async () => {
    saveBtn.disabled = true;
    saveBtn.textContent = "Saving...";
    try {
      const payload = {
        status: statusSelect.value,
        notes: notesInput.value,
      };
      const response = await fetch(`/api/trades/${encodeURIComponent(trade.id)}`, {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payload),
      });
      if (!response.ok) {
        throw new Error("Save failed");
      }
      saveBtn.textContent = "Saved";
      setTimeout(() => {
        saveBtn.textContent = "Save";
      }, 1200);
    } catch (err) {
      console.error(err);
      saveBtn.textContent = "Retry";
    } finally {
      saveBtn.disabled = false;
    }
  });

  return fragment;
}

function renderTrades(trades) {
  tradesBody.innerHTML = "";
  for (const trade of trades) {
    tradesBody.appendChild(buildRow(trade));
  }
}

async function refresh() {
  try {
    const data = await loadDashboard();
    setSummary(data.summary);
    renderTrades(data.trades);
  } catch (err) {
    console.error(err);
    tradesBody.innerHTML =
      '<tr><td colspan="10">Failed to load trade data. Check server logs.</td></tr>';
  }
}

refreshBtn.addEventListener("click", refresh);
searchInput.addEventListener("input", () => {
  window.clearTimeout(searchInput._debounce);
  searchInput._debounce = window.setTimeout(refresh, 200);
});
statusFilter.addEventListener("change", refresh);

refresh();
