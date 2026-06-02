/* ============================================================
   Kalshi Weather Bot — dashboard client
   Fetches /api/trades + /api/health, computes KPIs, renders
   hand-rolled SVG charts and a filterable activity feed.
   ============================================================ */

const SVG_NS = "http://www.w3.org/2000/svg";

const els = {
  refreshBtn: document.getElementById("refreshBtn"),
  statusChip: document.getElementById("statusChip"),
  sigmaChip: document.getElementById("sigmaChip"),
  latestBody: document.getElementById("latestBody"),
  kpiMade: document.getElementById("kpiMade"),
  kpiMadeSub: document.getElementById("kpiMadeSub"),
  kpiHeld: document.getElementById("kpiHeld"),
  kpiClosed: document.getElementById("kpiClosed"),
  kpiExposure: document.getElementById("kpiExposure"),
  kpiExposureSub: document.getElementById("kpiExposureSub"),
  kpiPnl: document.getElementById("kpiPnl"),
  curveChart: document.getElementById("curveChart"),
  curveTag: document.getElementById("curveTag"),
  cityChart: document.getElementById("cityChart"),
  cityTotal: document.getElementById("cityTotal"),
  feed: document.getElementById("feed"),
  activityCount: document.getElementById("activityCount"),
  navTradesCount: document.getElementById("navTradesCount"),
  navActivityCount: document.getElementById("navActivityCount"),
  searchInput: document.getElementById("searchInput"),
  statusFilter: document.getElementById("statusFilter"),
  rowTemplate: document.getElementById("rowTemplate"),
};

const state = {
  trades: [],
  search: "",
  status: "",
};

/* ---------- formatters ---------- */
const num = (v) => (v === null || v === undefined ? null : Number(v));
const pct = (v) => (v === null ? "–" : `${Math.round(num(v) * 100)}%`);
const cents = (v) => (v === null ? "–" : `${Math.round(num(v) * 100)}¢`);
const usd = (v) => {
  if (v === null || v === undefined) return "$0.00";
  const n = Number(v);
  return `${n < 0 ? "-" : ""}$${Math.abs(n).toFixed(2)}`;
};
const signedPts = (v) => {
  if (v === null || v === undefined) return "–";
  const n = Number(v) * 100;
  return `${n >= 0 ? "+" : ""}${n.toFixed(1)}`;
};

function timeAgo(iso) {
  if (!iso) return "";
  const then = new Date(iso).getTime();
  const secs = Math.max(0, Math.floor((Date.now() - then) / 1000));
  if (secs < 60) return `${secs}s ago`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  return `${days}d ago`;
}

/* ---------- KPI icons (inline SVG, no emoji) ---------- */
const ICONS = {
  made: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 17l5-5 4 3 7-8"/><path d="M16 4h5v5"/></svg>',
  held: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="8"/><path d="M12 8v4l3 2"/></svg>',
  closed: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M5 13l4 4L19 7"/></svg>',
  exposure: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M3 10h18"/></svg>',
  pnl: '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 1v22"/><path d="M17 5H9.5a3.5 3.5 0 0 0 0 7h5a3.5 3.5 0 0 1 0 7H6"/></svg>',
};
function injectKpiIcons() {
  document.querySelectorAll(".kpi-ico").forEach((el) => {
    const key = el.getAttribute("data-ico");
    if (ICONS[key]) el.innerHTML = ICONS[key];
  });
}

/* ---------- data ---------- */
async function fetchAll() {
  const res = await fetch("/api/trades");
  if (!res.ok) throw new Error(`trades ${res.status}`);
  const data = await res.json();
  return data.trades || [];
}

async function pingHealth() {
  try {
    const res = await fetch("/api/health");
    setOnline(res.ok);
  } catch {
    setOnline(false);
  }
}

function setOnline(ok) {
  const chip = els.statusChip;
  chip.classList.toggle("is-down", !ok);
  chip.innerHTML = ok
    ? '<span class="dot dot-green pulse"></span>Online'
    : '<span class="dot dot-rose"></span>Offline';
}

/* ---------- derived metrics ---------- */
function computeKpis(trades) {
  let held = 0,
    closed = 0,
    watch = 0,
    exposure = 0,
    pnl = 0,
    contracts = 0;
  for (const t of trades) {
    if (t.status === "held") held++;
    else if (t.status === "closed") closed++;
    else if (t.status === "watch") watch++;
    const ct = Number(t.contracts) || 0;
    contracts += ct;
    if (t.limit_price != null) exposure += ct * Number(t.limit_price);
    if (t.net_ev_per_contract != null) pnl += ct * Number(t.net_ev_per_contract);
  }
  return { made: trades.length, held, closed, watch, exposure, pnl, contracts };
}

function renderKpis(k) {
  els.kpiMade.textContent = k.made;
  els.kpiMadeSub.textContent = k.watch ? `${k.watch} on watch` : "unique opportunities";
  els.kpiHeld.textContent = k.held;
  els.kpiClosed.textContent = k.closed;
  els.kpiExposure.textContent = usd(k.exposure);
  els.kpiExposureSub.textContent = `${k.contracts} contracts`;
  els.kpiPnl.textContent = usd(k.pnl);
  els.kpiPnl.classList.toggle("pos", k.pnl > 0);
  els.kpiPnl.classList.toggle("neg", k.pnl < 0);
  els.navTradesCount.textContent = k.made;
  els.navActivityCount.textContent = k.made;
}

function renderLatest(trades) {
  if (!trades.length) {
    els.latestBody.textContent = "No decisions logged yet.";
    return;
  }
  const t = [...trades].sort(
    (a, b) => new Date(b.last_seen) - new Date(a.last_seen)
  )[0];
  const side = (t.side || "").toUpperCase();
  els.latestBody.innerHTML =
    `<span class="mono">${escapeHtml(t.ticker)}</span> · ${escapeHtml(t.city)} — ` +
    `${side} model ${pct(t.model_p_yes)} vs market ${pct(t.market_p_implied)} · ` +
    `edge ${signedPts(t.raw_edge)} · ${timeAgo(t.last_seen)}`;
}

function renderSigmaChip(trades) {
  const sources = new Set(trades.map((t) => t.sigma_source).filter(Boolean));
  if (!sources.size) return;
  const label = sources.size === 1 ? [...sources][0] : "blended";
  els.sigmaChip.textContent = `σ · ${label.replace(/_/g, " ")}`;
}

/* ---------- charts ---------- */
function svgEl(name, attrs) {
  const el = document.createElementNS(SVG_NS, name);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, v);
  return el;
}

function renderCurve(trades) {
  const host = els.curveChart;
  host.innerHTML = "";
  const ordered = [...trades].sort(
    (a, b) => new Date(a.first_seen) - new Date(b.first_seen)
  );
  if (!ordered.length) {
    host.innerHTML = '<div class="chart-empty">No decisions in window</div>';
    els.curveTag.textContent = "$0.00";
    return;
  }

  // cumulative expected P&L
  let cum = 0;
  const pts = ordered.map((t) => {
    const ct = Number(t.contracts) || 0;
    cum += (Number(t.net_ev_per_contract) || 0) * ct;
    return { x: new Date(t.first_seen).getTime(), y: cum, t };
  });

  const W = 720,
    H = 240,
    padL = 44,
    padR = 16,
    padT = 16,
    padB = 26;
  const innerW = W - padL - padR;
  const innerH = H - padT - padB;
  const xs = pts.map((p) => p.x);
  const minX = Math.min(...xs);
  const maxX = Math.max(...xs);
  const maxY = Math.max(...pts.map((p) => p.y), 0.01);
  const minY = Math.min(...pts.map((p) => p.y), 0);
  const spanX = maxX - minX || 1;
  const spanY = maxY - minY || 1;

  const sx = (x) => padL + ((x - minX) / spanX) * innerW;
  const sy = (y) => padT + innerH - ((y - minY) / spanY) * innerH;

  const svg = svgEl("svg", { viewBox: `0 0 ${W} ${H}`, role: "img" });
  svg.setAttribute("aria-label", "Cumulative expected P&L over time");

  // gridlines + y labels
  const ticks = 4;
  for (let i = 0; i <= ticks; i++) {
    const yVal = minY + (spanY * i) / ticks;
    const y = sy(yVal);
    svg.appendChild(svgEl("line", { class: "grid-line", x1: padL, y1: y, x2: W - padR, y2: y }));
    const lbl = svgEl("text", { class: "axis-label", x: padL - 8, y: y + 3, "text-anchor": "end" });
    lbl.textContent = usd(yVal);
    svg.appendChild(lbl);
  }

  // gradient defs
  const defs = svgEl("defs", {});
  const grad = svgEl("linearGradient", { id: "curveFill", x1: 0, y1: 0, x2: 0, y2: 1 });
  grad.appendChild(svgEl("stop", { offset: "0%", "stop-color": "#38bdf8", "stop-opacity": 0.35 }));
  grad.appendChild(svgEl("stop", { offset: "100%", "stop-color": "#38bdf8", "stop-opacity": 0 }));
  defs.appendChild(grad);
  svg.appendChild(defs);

  const linePath = pts.map((p, i) => `${i ? "L" : "M"}${sx(p.x).toFixed(1)} ${sy(p.y).toFixed(1)}`).join(" ");
  const areaPath = `${linePath} L${sx(pts[pts.length - 1].x).toFixed(1)} ${sy(minY).toFixed(1)} L${sx(pts[0].x).toFixed(1)} ${sy(minY).toFixed(1)} Z`;

  svg.appendChild(svgEl("path", { d: areaPath, fill: "url(#curveFill)" }));
  svg.appendChild(
    svgEl("path", { d: linePath, fill: "none", stroke: "#38bdf8", "stroke-width": 2.2, "stroke-linejoin": "round", "stroke-linecap": "round" })
  );

  // points
  for (const p of pts) {
    const c = svgEl("circle", { cx: sx(p.x), cy: sy(p.y), r: 3.2, fill: "#0a0f1c", stroke: "#38bdf8", "stroke-width": 1.8 });
    const title = svgEl("title", {});
    title.textContent = `${p.t.ticker} · ${usd(p.y)} cumulative`;
    c.appendChild(title);
    svg.appendChild(c);
  }

  // x axis end labels
  const fmtTime = (ms) => new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric" });
  const x0 = svgEl("text", { class: "axis-label", x: padL, y: H - 8, "text-anchor": "start" });
  x0.textContent = fmtTime(minX);
  const x1 = svgEl("text", { class: "axis-label", x: W - padR, y: H - 8, "text-anchor": "end" });
  x1.textContent = fmtTime(maxX);
  svg.appendChild(x0);
  svg.appendChild(x1);

  host.appendChild(svg);
  els.curveTag.textContent = usd(cum);
  els.curveTag.style.color = cum >= 0 ? "var(--green)" : "var(--rose)";
}

function renderCityBars(trades) {
  const host = els.cityChart;
  host.innerHTML = "";
  const byCity = new Map();
  for (const t of trades) {
    byCity.set(t.city, (byCity.get(t.city) || 0) + 1);
  }
  const rows = [...byCity.entries()].sort((a, b) => b[1] - a[1]);
  els.cityTotal.textContent = `${rows.length} cities · ${trades.length} markets`;
  if (!rows.length) {
    host.innerHTML = '<div class="chart-empty">No markets</div>';
    return;
  }

  const rowH = 30,
    gap = 10,
    labelW = 96,
    valW = 30,
    W = 360;
  const H = rows.length * (rowH + gap);
  const maxV = Math.max(...rows.map((r) => r[1]));
  const barMax = W - labelW - valW - 10;

  const svg = svgEl("svg", { viewBox: `0 0 ${W} ${H}`, role: "img" });
  svg.setAttribute("aria-label", "Trades by city");

  rows.forEach(([city, count], i) => {
    const y = i * (rowH + gap);
    const cy = y + rowH / 2;
    const w = Math.max(6, (count / maxV) * barMax);

    const label = svgEl("text", { class: "bar-label", x: 0, y: cy + 4 });
    label.textContent = city.length > 12 ? city.slice(0, 11) + "…" : city;
    svg.appendChild(label);

    svg.appendChild(svgEl("rect", { x: labelW, y, width: barMax, height: rowH, rx: 6, fill: "rgba(255,255,255,0.03)" }));
    const bar = svgEl("rect", { x: labelW, y, width: w, height: rowH, rx: 6, fill: "url(#barFill)" });
    svg.appendChild(bar);

    const val = svgEl("text", { class: "bar-value", x: labelW + w + 8, y: cy + 4 });
    val.textContent = count;
    svg.appendChild(val);
  });

  const defs = svgEl("defs", {});
  const grad = svgEl("linearGradient", { id: "barFill", x1: 0, y1: 0, x2: 1, y2: 0 });
  grad.appendChild(svgEl("stop", { offset: "0%", "stop-color": "#1e40af" }));
  grad.appendChild(svgEl("stop", { offset: "100%", "stop-color": "#38bdf8" }));
  defs.appendChild(grad);
  svg.appendChild(defs);

  host.appendChild(svg);
}

/* ---------- activity feed ---------- */
function filteredTrades() {
  let list = [...state.trades].sort(
    (a, b) => new Date(b.last_seen) - new Date(a.last_seen)
  );
  if (state.status) list = list.filter((t) => t.status === state.status);
  if (state.search) {
    const q = state.search.toLowerCase();
    list = list.filter(
      (t) =>
        (t.ticker || "").toLowerCase().includes(q) ||
        (t.city || "").toLowerCase().includes(q) ||
        (t.side || "").toLowerCase().includes(q)
    );
  }
  return list;
}

function sideClass(side) {
  const s = (side || "").toLowerCase();
  if (s === "yes") return "side-yes";
  if (s === "no") return "side-no";
  return "side-unknown";
}

function buildRow(t) {
  const frag = els.rowTemplate.content.cloneNode(true);
  const row = frag.querySelector(".trade");
  row.dataset.id = t.id;

  const badge = row.querySelector(".side-badge");
  badge.textContent = (t.side || "—").toUpperCase();
  badge.classList.add(sideClass(t.side));

  row.querySelector(".ticker").textContent = t.ticker;
  row.querySelector(".trade-meta").textContent =
    `${t.city} · resolves ${t.resolution_date} · ${timeAgo(t.last_seen)}` +
    (t.emissions > 1 ? ` · ${t.emissions}× seen` : "");

  row.querySelector(".prob-model").textContent = pct(t.model_p_yes);
  row.querySelector(".prob-mkt").textContent = pct(t.market_p_implied);

  const edgeEl = row.querySelector(".edge");
  edgeEl.textContent = signedPts(t.raw_edge);
  if (t.raw_edge != null) edgeEl.classList.add(Number(t.raw_edge) >= 0 ? "pos" : "neg");

  row.querySelector(".size").textContent = `${t.contracts} @ ${cents(t.limit_price)}`;

  const evEl = row.querySelector(".ev");
  evEl.textContent = t.net_ev_per_contract != null ? usd(t.net_ev_per_contract) : "–";
  if (t.net_ev_per_contract != null)
    evEl.classList.add(Number(t.net_ev_per_contract) >= 0 ? "pos" : "neg");

  const chip = row.querySelector(".status-chip");
  chip.textContent = t.status;
  chip.classList.add(`status-${t.status}`);

  // editing
  const editToggle = row.querySelector(".editToggle");
  const editPanel = row.querySelector(".trade-edit");
  const statusSelect = row.querySelector(".statusSelect");
  const notesInput = row.querySelector(".notesInput");
  const saveBtn = row.querySelector(".saveBtn");

  statusSelect.value = t.status;
  notesInput.value = t.notes || "";

  editToggle.addEventListener("click", () => {
    const open = !editPanel.hidden;
    editPanel.hidden = open;
    editToggle.textContent = open ? "Edit" : "Close";
  });

  saveBtn.addEventListener("click", async () => {
    saveBtn.disabled = true;
    saveBtn.textContent = "Saving…";
    try {
      const res = await fetch(`/api/trades/${encodeURIComponent(t.id)}`, {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ status: statusSelect.value, notes: notesInput.value }),
      });
      if (!res.ok) throw new Error("save failed");
      // update local model + re-render so KPIs/chips stay in sync
      t.status = statusSelect.value;
      t.notes = notesInput.value;
      saveBtn.textContent = "Saved ✓";
      setTimeout(() => {
        renderKpis(computeKpis(state.trades));
        renderFeed();
      }, 500);
    } catch (err) {
      console.error(err);
      saveBtn.textContent = "Retry";
      saveBtn.disabled = false;
    }
  });

  return frag;
}

function renderFeed() {
  const list = filteredTrades();
  els.activityCount.textContent = `${list.length} of ${state.trades.length} events`;
  els.feed.innerHTML = "";
  if (!list.length) {
    els.feed.innerHTML = '<div class="chart-empty">No matching trades.</div>';
    return;
  }
  const frag = document.createDocumentFragment();
  for (const t of list) frag.appendChild(buildRow(t));
  els.feed.appendChild(frag);
}

/* ---------- orchestration ---------- */
function renderAll() {
  const k = computeKpis(state.trades);
  renderKpis(k);
  renderLatest(state.trades);
  renderSigmaChip(state.trades);
  renderCurve(state.trades);
  renderCityBars(state.trades);
  renderFeed();
}

async function refresh() {
  els.refreshBtn.classList.add("spin");
  try {
    const [trades] = await Promise.all([fetchAll(), pingHealth()]);
    state.trades = trades;
    renderAll();
  } catch (err) {
    console.error(err);
    setOnline(false);
    els.feed.innerHTML =
      '<div class="chart-empty">Failed to load trade data. Check the dashboard server logs.</div>';
  } finally {
    els.refreshBtn.classList.remove("spin");
  }
}

function escapeHtml(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
  );
}

/* ---------- events ---------- */
els.refreshBtn.addEventListener("click", refresh);
els.searchInput.addEventListener("input", () => {
  clearTimeout(els.searchInput._t);
  els.searchInput._t = setTimeout(() => {
    state.search = els.searchInput.value.trim();
    renderFeed();
  }, 160);
});
els.statusFilter.addEventListener("click", (e) => {
  const btn = e.target.closest(".seg");
  if (!btn) return;
  els.statusFilter.querySelectorAll(".seg").forEach((b) => b.classList.remove("is-active"));
  btn.classList.add("is-active");
  state.status = btn.dataset.status || "";
  renderFeed();
});

// sidebar nav scroll (single page — anchors to sections)
document.querySelectorAll(".nav-item").forEach((item) => {
  item.addEventListener("click", () => {
    document.querySelectorAll(".nav-item").forEach((n) => n.classList.remove("is-active"));
    item.classList.add("is-active");
  });
});

injectKpiIcons();
refresh();
// auto-refresh every 30s, but don't yank the feed out from under an
// open edit panel or a focused input (search / notes mid-typing).
setInterval(() => {
  const editing = document.querySelector(".trade-edit:not([hidden])");
  const typing = document.activeElement && ["INPUT", "SELECT", "TEXTAREA"].includes(document.activeElement.tagName);
  if (editing || typing) return;
  refresh();
}, 30000);
