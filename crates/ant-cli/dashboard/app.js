// Ant local dashboard (D-042). All data comes from this origin's api/builds
// and is rendered through DOM APIs with textContent (never innerHTML).
"use strict";

const REFRESH_MS = 3000;
const DAY = 86400000;
const SVG = "http://www.w3.org/2000/svg";

const state = {
  builds: [],
  running: [],
  clockOffset: 0, // server clock minus browser clock
  now: Date.now(),
  page: "overview",
  filter: "all",
  query: "",
  project: null, // project path the builds page is narrowed to
  expanded: new Set(),
  lastOk: 0,
  loaded: false,
  signature: "",
  renderedAt: 0,
};

const $ = (id) => document.getElementById(id);

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = text;
  return node;
}

function svgEl(tag, attrs) {
  const node = document.createElementNS(SVG, tag);
  for (const [key, value] of Object.entries(attrs || {})) node.setAttribute(key, String(value));
  return node;
}

function icon(path) {
  const svg = svgEl("svg", { viewBox: "0 0 24 24", "aria-hidden": "true" });
  svg.append(svgEl("path", { d: path }));
  return svg;
}

// ---------- formatting ----------

function projectName(path) {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.length ? parts[parts.length - 1] : path;
}

function duration(ms) {
  if (ms < 1000) return `${Math.max(0, Math.round(ms))} ms`;
  const s = ms / 1000;
  if (s < 60) return `${s < 10 ? s.toFixed(1) : Math.round(s)} s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min ${Math.round(s % 60)} s`;
  return `${Math.floor(m / 60)} h ${m % 60} min`;
}

function ago(ms, now) {
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 45) return "just now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h} h ago`;
  const d = Math.round(h / 24);
  if (d < 14) return `${d} d ago`;
  return new Date(ms).toLocaleDateString();
}

function bytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function percent(part, whole) {
  return whole ? `${Math.round((part / whole) * 100)}%` : "–";
}

const TOOL_LABELS = {
  cargo: ["Rs", "Rust · cargo"], go: ["Go", "Go"], npm: ["JS", "npm"], cmake: ["C", "CMake"],
  gradle: ["An", "Android · Gradle"], flutter: ["Fl", "Flutter"], xcode: ["iOS", "Xcode"],
  maven: ["Mv", "Maven"], python: ["Py", "Python"], tauri: ["Ta", "Tauri"],
  electron: ["El", "Electron"], docker: ["Dk", "Docker"],
};

function toolBadge(adapter, withLabel = true) {
  const [short, label] = TOOL_LABELS[adapter] || [adapter.slice(0, 2), adapter];
  const wrap = el("span", "tool");
  const dot = el("span", `tool-dot ${TOOL_LABELS[adapter] ? `t-${adapter}` : "t-other"}`, short);
  dot.setAttribute("aria-hidden", "true");
  wrap.append(dot);
  if (withLabel) wrap.append(el("span", "", label));
  else wrap.setAttribute("title", label);
  return wrap;
}

// ---------- derived data ----------

function outcome(build) {
  if (!build.succeeded) return "failed";
  const ran = build.steps.some((s) => s.status === "succeeded");
  const reused = build.steps.some((s) => s.status === "reused");
  return reused && !ran ? "reused" : "built";
}

const OUTCOME_LABEL = { built: "Built", reused: "Reused", failed: "Failed" };

function statusBadge(build) {
  const kind = outcome(build);
  return el("span", `status ${kind}`, OUTCOME_LABEL[kind]);
}

function matchesQuery(build) {
  if (!state.query) return true;
  const q = state.query.toLowerCase();
  const tool = (TOOL_LABELS[build.adapter] || ["", build.adapter])[1].toLowerCase();
  return build.project.toLowerCase().includes(q) || build.adapter.includes(q) || tool.includes(q);
}

function visibleBuilds() {
  return state.builds.filter(matchesQuery);
}

// Time saved by reuse: for each fully reused build, the duration of the
// most recent earlier successful build of the same project that really ran,
// minus what the reuse took. An estimate, labeled as such.
function timeSaved(builds) {
  let saved = 0;
  let counted = 0;
  const ordered = [...builds].sort((a, b) => a.started_ms - b.started_ms);
  const lastRun = new Map();
  for (const build of ordered) {
    const kind = outcome(build);
    if (kind === "built") lastRun.set(build.project, build.millis);
    else if (kind === "reused" && lastRun.has(build.project)) {
      saved += Math.max(0, lastRun.get(build.project) - build.millis);
      counted += 1;
    }
  }
  return { saved, counted };
}

function projects(builds) {
  const map = new Map();
  for (const build of builds) {
    if (!map.has(build.project)) map.set(build.project, []);
    map.get(build.project).push(build);
  }
  return [...map.entries()].map(([path, items]) => ({ path, items, latest: items[0] }));
}

function currentFiles(builds) {
  const files = [];
  for (const build of builds) {
    for (const file of build.downloads) {
      if (!file.replaced) files.push({ file, build });
    }
  }
  return files;
}

// ---------- charts ----------

function sparkline(svg, values, { failed = [] } = {}) {
  svg.replaceChildren();
  if (values.length === 0) return;
  const width = 120;
  const height = 32;
  const max = Math.max(...values, 1);
  const step = values.length > 1 ? width / (values.length - 1) : 0;
  const points = values.map((v, i) => [values.length > 1 ? i * step : width / 2, height - 3 - (v / max) * (height - 6)]);
  const line = points.map(([x, y], i) => `${i ? "L" : "M"}${x.toFixed(1)} ${y.toFixed(1)}`).join(" ");
  svg.append(svgEl("path", { class: "area", d: `${line} L${points[points.length - 1][0].toFixed(1)} ${height} L${points[0][0].toFixed(1)} ${height} Z` }));
  svg.append(svgEl("path", { class: "line", d: line }));
  failed.forEach((isFailed, i) => {
    if (isFailed) svg.append(svgEl("circle", { class: "point-failed", cx: points[i][0], cy: points[i][1], r: 2.6 }));
  });
}

function activityChart(builds, now) {
  const svg = $("chart");
  svg.replaceChildren();
  const days = 14;
  const startOfToday = new Date(now);
  startOfToday.setHours(0, 0, 0, 0);
  const buckets = Array.from({ length: days }, (_, i) => ({
    start: startOfToday.getTime() - (days - 1 - i) * DAY, built: 0, reused: 0, failed: 0,
  }));
  for (const build of builds) {
    const index = Math.floor((build.started_ms - buckets[0].start) / DAY);
    if (index >= 0 && index < days) buckets[index][outcome(build)] += 1;
  }
  const W = 640, H = 220, left = 32, right = 8, top = 10, bottom = 26;
  const plotW = W - left - right, plotH = H - top - bottom;
  const peak = Math.max(1, ...buckets.map((b) => b.built + b.reused + b.failed));
  const niceMax = peak <= 4 ? 4 : Math.ceil(peak / 4) * 4;
  for (let i = 0; i <= 4; i += 1) {
    const y = top + plotH - (plotH * i) / 4;
    svg.append(svgEl("line", { class: "grid", x1: left, x2: W - right, y1: y, y2: y }));
    const label = svgEl("text", { class: "axis", x: left - 8, y: y + 4, "text-anchor": "end" });
    label.textContent = String((niceMax * i) / 4);
    svg.append(label);
  }
  const slot = plotW / days;
  const barW = Math.min(26, slot * 0.62);
  let total = 0;
  buckets.forEach((bucket, i) => {
    const x = left + i * slot + (slot - barW) / 2;
    let y = top + plotH;
    for (const kind of ["built", "reused", "failed"]) {
      const count = bucket[kind];
      if (!count) continue;
      total += count;
      const h = (count / niceMax) * plotH;
      y -= h;
      const rect = svgEl("rect", { class: `bar-${kind}`, x: x.toFixed(1), y: y.toFixed(1), width: barW.toFixed(1), height: Math.max(h - 1, 1).toFixed(1), rx: 3 });
      const title = svgEl("title");
      title.textContent = `${new Date(bucket.start).toLocaleDateString()}: ${count} ${OUTCOME_LABEL[kind].toLowerCase()}`;
      rect.append(title);
      svg.append(rect);
    }
    if (i % 2 === (days - 1) % 2) {
      const label = svgEl("text", { class: i === days - 1 ? "axis today" : "axis", x: (x + barW / 2).toFixed(1), y: H - 8, "text-anchor": "middle" });
      label.textContent = i === days - 1 ? "Today" : new Date(bucket.start).toLocaleDateString(undefined, { month: "short", day: "numeric" });
      svg.append(label);
    }
  });
  $("chart-desc").textContent = `${total} builds in the last 14 days.`;
}

// ---------- tables ----------

function buildsTable(table, builds, { expandable = false, limit = Infinity } = {}) {
  table.replaceChildren();
  const head = el("thead");
  const headRow = el("tr");
  const columns = [
    ...(expandable ? [["", ""]] : []),
    ["Status", ""], ["Project", ""], ["Toolchain", "hide-sm"], ["Duration", "num"],
    ["Steps", "hide-sm"], ["Started", "num"],
  ];
  for (const [label, cls] of columns) {
    const th = el("th", cls, label);
    th.scope = "col";
    if (!label) th.append(el("span", "sr-only", "Details"));
    headRow.append(th);
  }
  head.append(headRow);
  const body = el("tbody");
  const shown = builds.slice(0, limit);
  if (shown.length === 0) {
    const row = el("tr");
    const cell = el("td", "none", state.builds.length ? "No builds match." : "No builds yet.");
    cell.colSpan = columns.length;
    row.append(cell);
    body.append(row);
  }
  for (const build of shown) {
    const row = el("tr", "row");
    const detailId = `detail-${build.id}`;
    if (expandable) {
      const cell = el("td");
      const button = el("button", "expand");
      button.type = "button";
      const open = state.expanded.has(build.id);
      button.setAttribute("aria-expanded", String(open));
      button.setAttribute("aria-controls", detailId);
      button.dataset.build = build.id;
      button.setAttribute("aria-label", `Details for the ${projectName(build.project)} build from ${new Date(build.started_ms).toLocaleString()}`);
      button.append(icon("m9 6 6 6-6 6"));
      button.addEventListener("click", () => {
        if (state.expanded.has(build.id)) state.expanded.delete(build.id);
        else state.expanded.add(build.id);
        render();
      });
      cell.append(button);
      row.append(cell);
    }
    const status = el("td");
    status.append(statusBadge(build));
    row.append(status);
    const project = el("td", "cell-project", projectName(build.project));
    project.append(el("span", "cell-sub", build.flavor || build.adapter));
    row.append(project);
    const tool = el("td", "hide-sm");
    tool.append(toolBadge(build.adapter));
    row.append(tool);
    row.append(el("td", "num", duration(build.millis)));
    const ran = build.steps.filter((s) => s.status === "succeeded").length;
    const reused = build.steps.filter((s) => s.status === "reused").length;
    const failed = build.steps.filter((s) => s.status !== "succeeded" && s.status !== "reused").length;
    const parts = [];
    if (ran) parts.push(`${ran} ran`);
    if (reused) parts.push(`${reused} reused`);
    if (failed) parts.push(`${failed} failed`);
    row.append(el("td", "hide-sm muted", parts.join(" · ") || "–"));
    const started = el("td", "num muted");
    const time = el("time", "", ago(build.started_ms, state.now));
    time.dateTime = new Date(build.started_ms).toISOString();
    time.title = new Date(build.started_ms).toLocaleString();
    started.append(time);
    row.append(started);
    body.append(row);
    if (expandable && state.expanded.has(build.id)) body.append(detailRow(build, columns.length, detailId));
  }
  table.append(head, body);
}

function detailRow(build, span, id) {
  const row = el("tr", "detail");
  row.id = id;
  const cell = el("td");
  cell.colSpan = span;
  const grid = el("div", "detail-grid");
  const stepsBox = el("div");
  stepsBox.append(el("h3", "", "Steps"));
  const steps = el("ul", "steps");
  for (const step of build.steps) {
    const item = el("li", "step");
    const kind = step.status === "succeeded" ? "built" : step.status === "reused" ? "reused" : "failed";
    const label = { succeeded: "Ran", reused: "Reused", failed: "Failed", skipped: "Skipped", timed_out: "Timed out", cancelled: "Cancelled" }[step.status] || step.status;
    item.append(el("span", `status ${kind}`, label), el("span", "step-name", step.id), el("span", "step-time", step.status === "reused" ? "from cache" : duration(step.millis)));
    steps.append(item);
  }
  if (!build.steps.length) steps.append(el("li", "muted", "No steps ran."));
  stepsBox.append(steps);
  const filesBox = el("div");
  filesBox.append(el("h3", "", "Deliverables"));
  filesBox.append(fileList(build.downloads.map((file) => ({ file, build })), true));
  const where = el("p", "cell-sub", build.project);
  stepsBox.append(where);
  grid.append(stepsBox, filesBox);
  cell.append(grid);
  const output = outputSection(build.id);
  output.classList.add("detail-output");
  cell.append(output);
  row.append(cell);
  return row;
}

function fileList(entries, compact = false) {
  const list = el("ul", compact ? "steps" : "files");
  if (!entries.length) list.append(el("li", compact ? "muted" : "none", compact ? "None collected." : "Nothing to download yet."));
  for (const { file, build } of entries) {
    const item = el("li", compact ? "step" : "file");
    const extension = (file.name.split(".").pop() || "file").slice(0, 4);
    if (!compact) item.append(el("span", "file-type", file.name.includes(".") ? extension : "bin"));
    const body = el("div", "file-body");
    body.append(el("span", compact ? "step-name" : "file-name", file.name));
    if (!compact) {
      const size = file.exists && !file.replaced ? `${bytes(file.bytes)} · ` : "";
      body.append(el("span", "file-meta", `${size}${projectName(build.project)} · ${build.adapter}${build.flavor ? ` · ${build.flavor}` : ""} · built ${ago(build.started_ms, state.now)}`));
    }
    item.append(body);
    if (file.replaced) {
      item.append(el("span", "file-gone", "Replaced by a newer build"));
    } else if (!file.exists) {
      item.append(el("span", "file-gone", "No longer on disk"));
    } else {
      const link = el("a", compact ? "link" : "btn primary");
      link.href = file.url;
      link.setAttribute("download", file.name);
      link.setAttribute("aria-label", `Download ${file.name}, ${bytes(file.bytes)}`);
      if (!compact) link.append(icon("M12 3v11m0 0-4-4m4 4 4-4M5 17v2a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2v-2"));
      link.append(document.createTextNode(compact ? `Download · ${bytes(file.bytes)}` : "Download"));
      if (!compact && file.name.toLowerCase().endsWith(".apk")) {
        const sharing = activeShare && activeShare.file === file.name;
        const phone = el("button", "btn", sharing ? "Sharing · show code" : "Install on phone");
        phone.type = "button";
        phone.dataset.share = file.url;
        phone.prepend(icon("M8 2h8a2 2 0 0 1 2 2v16a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2zm3 17h2"));
        phone.setAttribute("aria-label", sharing ? `Show the install code for ${file.name}` : `Install ${file.name} on a phone`);
        phone.addEventListener("click", () => {
          if (sharing) showShare(activeShare, null);
          else startShare(build.id, Number(file.url.split("/").pop()), file.name);
        });
        item.append(phone);
      }
      item.append(link);
    }
    list.append(item);
  }
  return list;
}

// ---------- pages ----------

function renderOverview(builds) {
  const now = state.now;
  const week = builds.filter((b) => b.started_ms >= now - 7 * DAY);
  const previous = builds.filter((b) => b.started_ms < now - 7 * DAY && b.started_ms >= now - 14 * DAY);
  $("kpi-builds").textContent = String(week.length);
  $("kpi-builds-sub").textContent = previous.length
    ? `${week.length >= previous.length ? "+" : ""}${week.length - previous.length} vs previous 7 days`
    : `${builds.length} in history`;
  const perDay = Array.from({ length: 7 }, (_, i) => {
    const start = now - (7 - i) * DAY;
    return week.filter((b) => b.started_ms >= start && b.started_ms < start + DAY).length;
  });
  sparkline($("spark-builds"), perDay);

  const ok = builds.filter((b) => b.succeeded).length;
  $("kpi-success").textContent = percent(ok, builds.length);
  $("kpi-success-sub").textContent = builds.length ? `${builds.length - ok} failed of ${builds.length}` : "No builds yet";
  $("meter-success").style.width = builds.length ? `${(ok / builds.length) * 100}%` : "0";

  const steps = builds.flatMap((b) => b.steps);
  const reused = steps.filter((s) => s.status === "reused").length;
  $("kpi-reuse").textContent = percent(reused, steps.length);
  $("kpi-reuse-sub").textContent = steps.length ? `${reused} of ${steps.length} steps came from the cache` : "No steps yet";
  $("meter-reuse").style.width = steps.length ? `${(reused / steps.length) * 100}%` : "0";

  const { saved, counted } = timeSaved(builds);
  $("kpi-saved").textContent = counted ? duration(saved) : "–";
  $("kpi-saved-sub").textContent = counted
    ? `≈ across ${counted} reused build${counted === 1 ? "" : "s"}, vs. each project's last real build`
    : "Shows once a build is reused";

  activityChart(builds, now);

  const list = $("overview-projects");
  list.replaceChildren();
  const top = projects(builds).slice(0, 6);
  if (!top.length) list.append(el("li", "none", "No projects yet."));
  for (const project of top) {
    const item = el("li");
    const button = el("button", "mini-project");
    button.type = "button";
    button.append(toolBadge(project.latest.adapter, false));
    const body = el("span", "mini-body");
    body.append(el("span", "mini-name", projectName(project.path)));
    body.append(el("span", "mini-meta", `${project.items.length} build${project.items.length === 1 ? "" : "s"} · ${percent(project.items.filter((b) => b.succeeded).length, project.items.length)} succeeded`));
    button.append(body, statusBadge(project.latest), el("span", "mini-time", ago(project.latest.started_ms, now)));
    button.addEventListener("click", () => showProject(project.path));
    item.append(button);
    list.append(item);
  }

  buildsTable($("overview-builds"), builds, { limit: 6 });
}

// ---------- live builds ----------

function serverNow() {
  return Date.now() + state.clockOffset;
}

function renderRunning() {
  const card = $("live-builds");
  const list = $("running-list");
  list.replaceChildren();
  card.hidden = state.running.length === 0;
  for (const build of state.running) {
    const item = el("li", "running-item");
    const top = el("div", "running-top");
    top.append(toolBadge(build.adapter, false));
    const title = el("div", "running-title");
    title.append(el("span", "running-name", projectName(build.project)));
    const done = build.steps.length;
    const total = Math.max(build.total_steps, done + build.current.length, 1);
    const now = build.current.length ? `now: ${build.current.join(", ")}` : "finishing";
    title.append(el("span", "running-meta",
      `${build.adapter}${build.flavor ? ` · ${build.flavor}` : ""} · step ${Math.min(done + 1, total)} of ${total} · ${now}`));
    top.append(title);
    const timer = el("span", "running-timer", duration(Math.max(0, serverNow() - build.started_ms)));
    timer.dataset.since = String(build.started_ms);
    timer.setAttribute("aria-label", "Elapsed time");
    top.append(timer);
    item.append(top);
    const bar = el("div", "meter meter-brand");
    bar.setAttribute("role", "progressbar");
    bar.setAttribute("aria-label", `${projectName(build.project)} progress`);
    bar.setAttribute("aria-valuemin", "0");
    bar.setAttribute("aria-valuemax", String(total));
    const fill = el("span");
    if (done === 0) {
      // Nothing finished yet: show activity rather than a bar stuck at 0%.
      bar.classList.add("indeterminate");
      bar.setAttribute("aria-valuetext", "In progress");
    } else {
      bar.setAttribute("aria-valuenow", String(done));
      fill.style.width = `${(done / total) * 100}%`;
    }
    bar.append(fill);
    item.append(bar);
    if (build.steps.length) {
      const steps = el("p", "running-steps",
        build.steps.map((s) => `${s.id} ${s.status === "reused" ? "reused" : s.status === "succeeded" ? duration(s.millis) : s.status}`).join(" · "));
      item.append(steps);
    }
    item.append(outputSection(build.id, { live: true }));
    list.append(item);
  }
}

// Elapsed timers tick every second without re-rendering anything else.
setInterval(() => {
  updateShareExpiry();
  for (const timer of document.querySelectorAll("[data-since]")) {
    timer.textContent = duration(Math.max(0, serverNow() - Number(timer.dataset.since)));
  }
}, 1000);

// ---------- build output (D-045) ----------

const ESC = String.fromCharCode(27);
const BEL = String.fromCharCode(7);
// Control sequences other than colors (cursor moves, erase, titles).
const OTHER_ESCAPES = new RegExp(
  `${ESC}\\[[0-9;?]*[A-Za-ln-z]|${ESC}\\][^${BEL}${ESC}]*(?:${BEL}|${ESC}\\\\)|${ESC}[()][0-9A-Za-z]|${ESC}[=>78]`, "g");
const SGR = new RegExp(`${ESC}\\[([0-9;]*)m`, "g");

// Apply one SGR parameter list to a style state {fg, bold, dim}.
function applySgr(style, params) {
  const codes = params === "" ? [0] : params.split(";").map(Number);
  for (let i = 0; i < codes.length; i += 1) {
    const code = codes[i];
    if (code === 0) { style.fg = null; style.bold = false; style.dim = false; }
    else if (code === 1) style.bold = true;
    else if (code === 2) style.dim = true;
    else if (code === 22) { style.bold = false; style.dim = false; }
    else if ((code >= 30 && code <= 37) || (code >= 90 && code <= 97)) style.fg = code;
    else if (code === 39) style.fg = null;
    else if (code === 38 || code === 48) i += codes[i + 1] === 5 ? 2 : codes[i + 1] === 2 ? 4 : 0;
  }
}

// Render one line of terminal output into `target` (text nodes and spans
// with classes only; never HTML). Returns the style state after the line.
function renderTerminalLine(target, raw, style) {
  target.replaceChildren();
  let text = raw.replace(/\r+$/, "");
  const lastReturn = text.lastIndexOf("\r");
  if (lastReturn >= 0) text = text.slice(lastReturn + 1); // progress redraws
  text = text.replace(OTHER_ESCAPES, "");
  const current = { ...style };
  let last = 0;
  const push = (chunk) => {
    if (!chunk) return;
    if (current.fg === null && !current.bold && !current.dim) {
      target.append(document.createTextNode(chunk));
    } else {
      const classes = [];
      if (current.fg !== null) classes.push(`a${current.fg}`);
      if (current.bold) classes.push("ab");
      if (current.dim) classes.push("ad");
      target.append(el("span", classes.join(" "), chunk));
    }
  };
  for (const match of text.matchAll(SGR)) {
    push(text.slice(last, match.index));
    applySgr(current, match[1]);
    last = match.index + match[0].length;
  }
  push(text.slice(last));
  if (!target.childNodes.length) target.append(document.createTextNode(" "));
  return current;
}

class LogView {
  constructor(id) {
    this.id = id;
    this.next = 0;
    this.carry = "";
    this.style = { fg: null, bold: false, dim: false };
    this.lines = 0;
    this.loading = false;
    this.missing = false;
    this.pre = el("pre", "log");
    this.pre.tabIndex = 0;
    this.pre.setAttribute("aria-label", "Build output");
    this.body = el("div");
    this.partial = el("div", "log-line");
    this.status = el("div", "log-status", "Loading output…");
    this.pre.append(this.status, this.body, this.partial);
  }

  append(text) {
    const pre = this.pre;
    const atBottom = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 12;
    this.carry += text;
    const parts = this.carry.split("\n");
    this.carry = parts.pop();
    for (const part of parts) {
      const line = el("div", "log-line");
      this.style = renderTerminalLine(line, part, this.style);
      this.body.append(line);
      this.lines += 1;
      if (this.lines > 5000) {
        this.body.firstChild.remove();
        this.lines -= 1;
      }
    }
    renderTerminalLine(this.partial, this.carry, this.style);
    if (atBottom) pre.scrollTop = pre.scrollHeight;
  }

  // Fetch everything new (in chunks), then stop until asked again.
  async load() {
    if (this.loading) return;
    this.loading = true;
    try {
      for (let round = 0; round < 40; round += 1) {
        const response = await fetch(`api/log/${encodeURIComponent(this.id)}?from=${this.next}`, { cache: "no-store" });
        if (!response.ok) {
          if (this.next === 0) {
            this.missing = true;
            this.status.textContent = "No output was recorded for this build (it ran before output capture, or its log was pruned).";
          }
          break;
        }
        const data = await response.json();
        if (data.text) this.append(data.text);
        this.next = data.next;
        this.status.textContent = this.next === 0 ? "No output yet." : "";
        this.status.hidden = this.next > 0;
        if (this.next >= data.size) break;
      }
    } catch {
      this.status.textContent = "Could not load the output.";
    } finally {
      this.loading = false;
    }
  }
}

const logViews = new Map();
const openLogs = new Set();

function logView(id) {
  if (!logViews.has(id)) logViews.set(id, new LogView(id));
  return logViews.get(id);
}

// A "Show output" toggle plus the viewer; the viewer element is reused
// across re-renders so scroll position and content survive.
function outputSection(id, { live = false } = {}) {
  const box = el("div", "output");
  const toggle = el("button", "btn", openLogs.has(id) ? "Hide output" : live ? "Show live output" : "Show output");
  toggle.type = "button";
  toggle.setAttribute("aria-expanded", String(openLogs.has(id)));
  toggle.dataset.log = id;
  toggle.addEventListener("click", () => {
    if (openLogs.has(id)) openLogs.delete(id);
    else {
      openLogs.add(id);
      logView(id).load();
    }
    render();
  });
  box.append(toggle);
  if (openLogs.has(id)) box.append(logView(id).pre);
  return box;
}

// Refresh open viewers of running builds (called by the poll loop).
function followLiveLogs() {
  for (const build of state.running) {
    if (openLogs.has(build.id)) logView(build.id).load();
  }
}

function renderBuilds(builds) {
  const scoped = state.project ? builds.filter((b) => b.project === state.project) : builds;
  const counts = { all: scoped.length, built: 0, reused: 0, failed: 0 };
  for (const build of scoped) counts[outcome(build)] += 1;
  for (const key of Object.keys(counts)) $(`count-${key}`).textContent = String(counts[key]);
  for (const chip of document.querySelectorAll(".chip")) {
    chip.setAttribute("aria-pressed", String(chip.dataset.filter === state.filter));
  }
  const filtered = state.filter === "all" ? scoped : scoped.filter((b) => outcome(b) === state.filter);
  const scope = $("builds-scope");
  scope.replaceChildren();
  if (state.project) {
    scope.append(document.createTextNode(`Only ${projectName(state.project)} · `));
    const clear = el("button", "link", "show all projects");
    clear.type = "button";
    clear.classList.add("chip-clear");
    clear.addEventListener("click", () => {
      state.project = null;
      render();
    });
    scope.append(clear);
  } else {
    scope.textContent = "Newest first · select a row's arrow for steps and downloads";
  }
  buildsTable($("builds-table"), filtered, { expandable: true });
}

function renderProjects(builds) {
  const grid = $("project-grid");
  grid.replaceChildren();
  const all = projects(builds);
  if (!all.length) {
    grid.append(el("p", "none card", state.builds.length ? "No projects match." : "No projects yet."));
    return;
  }
  for (const project of all) {
    const card = el("article", "card project-card");
    const top = el("div", "project-top");
    top.append(toolBadge(project.latest.adapter, false));
    const title = el("div", "project-title");
    title.append(el("h2", "", projectName(project.path)));
    const path = el("p", "project-path", project.path);
    path.title = project.path;
    title.append(path);
    top.append(title, statusBadge(project.latest));
    card.append(top);

    const stats = el("div", "project-stats");
    const ok = project.items.filter((b) => b.succeeded);
    const typical = ok.filter((b) => outcome(b) === "built").map((b) => b.millis).sort((a, b) => a - b);
    for (const [label, value] of [
      ["Builds", String(project.items.length)],
      ["Success", percent(ok.length, project.items.length)],
      ["Typical build", typical.length ? duration(typical[Math.floor(typical.length / 2)]) : "–"],
    ]) {
      const stat = el("div", "stat");
      stat.append(el("p", "stat-label", label), el("p", "stat-value", value));
      stats.append(stat);
    }
    card.append(stats);

    const recent = project.items.slice(0, 20).reverse();
    const spark = svgEl("svg", { class: "project-spark", viewBox: "0 0 120 32", preserveAspectRatio: "none", "aria-hidden": "true" });
    sparkline(spark, recent.map((b) => b.millis), { failed: recent.map((b) => !b.succeeded) });
    card.append(spark);

    const foot = el("div", "project-foot");
    foot.append(el("span", "count-pill", `${(TOOL_LABELS[project.latest.adapter] || ["", project.latest.adapter])[1]} · last build ${ago(project.latest.started_ms, state.now)}`));
    const view = el("button", "btn", "View builds");
    view.type = "button";
    view.addEventListener("click", () => showProject(project.path));
    foot.append(view);
    card.append(foot);
    grid.append(card);
  }
}

// ---------- install on phone (D-046) ----------

let activeShare = null; // { file, url, qr, expires_ms } while sharing

async function shareRequest(path, body) {
  const response = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-Ant-Dashboard": "1" },
    body: body ? JSON.stringify(body) : "",
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json();
}

function drawQr(svg, rows) {
  svg.replaceChildren();
  const size = rows.length;
  const quiet = 4;
  const total = size + quiet * 2;
  svg.setAttribute("viewBox", `0 0 ${total} ${total}`);
  svg.append(svgEl("rect", { x: 0, y: 0, width: total, height: total, class: "qr-light" }));
  rows.forEach((row, y) => {
    // One rectangle per run of dark modules keeps the drawing small.
    let x = 0;
    while (x < row.length) {
      if (row[x] !== "1") { x += 1; continue; }
      let end = x;
      while (end < row.length && row[end] === "1") end += 1;
      svg.append(svgEl("rect", { x: x + quiet, y: y + quiet, width: end - x, height: 1, class: "qr-dark" }));
      x = end;
    }
  });
}

function showShare(share, error) {
  const dialog = $("share-dialog");
  activeShare = share;
  $("share-error").hidden = !error;
  $("share-error").textContent = error || "";
  $("share-body").hidden = !share;
  $("share-stop").hidden = !share;
  $("share-url").parentElement.hidden = !share;
  $("share-firewall").hidden = !share || !/Windows/i.test(navigator.userAgent);
  if (share) {
    $("share-file").textContent = share.file;
    drawQr($("share-qr"), share.qr);
    $("share-url").textContent = share.url;
    updateShareExpiry();
  } else {
    $("share-expiry").textContent = "";
  }
  if (!dialog.open) dialog.showModal();
  render();
}

function updateShareExpiry() {
  if (!activeShare) return;
  const left = activeShare.expires_ms - serverNow();
  if (left <= 0) {
    activeShare = null;
    $("share-expiry").textContent = "Sharing has stopped.";
    $("share-body").hidden = true;
    $("share-stop").hidden = true;
    render();
    return;
  }
  const minutes = Math.floor(left / 60000);
  const seconds = Math.floor((left % 60000) / 1000);
  $("share-expiry").textContent =
    `Sharing on your network for ${minutes}:${String(seconds).padStart(2, "0")} more. Only someone with this link can download it.`;
}

async function startShare(build, index, file) {
  $("share-file").textContent = file;
  try {
    const data = await shareRequest("api/share", { build, index });
    showShare(data.share, data.error);
  } catch (error) {
    showShare(null, `Could not start sharing (${error.message}). Is \`ant dashboard\` still running?`);
  }
}

$("share-stop").addEventListener("click", async () => {
  try { await shareRequest("api/share/stop"); } catch { /* the share expires on its own */ }
  activeShare = null;
  $("share-dialog").close();
  render();
});
$("share-done").addEventListener("click", () => $("share-dialog").close());
$("share-close").addEventListener("click", () => $("share-dialog").close());

// A share started earlier (another tab, or before a reload) is shown too.
fetch("api/share", { cache: "no-store" })
  .then((response) => (response.ok ? response.json() : null))
  .then((data) => { if (data && data.share) { activeShare = data.share; render(); } })
  .catch(() => {});

function renderDeliverables(builds) {
  const target = $("files");
  const entries = currentFiles(builds);
  target.replaceWith(Object.assign(fileList(entries), { id: "files" }));
}

function showProject(path) {
  state.project = path;
  state.filter = "all";
  location.hash = "#builds";
  render();
}

// ---------- shell ----------

const TITLES = { overview: "Overview", builds: "Builds", projects: "Projects", deliverables: "Deliverables" };

// Re-rendering replaces elements; keep keyboard focus on the "same" control
// (identified by its id, data attributes, or label) across renders.
function focusKey(node) {
  if (!node || node === document.body) return null;
  return node.id ? `#${node.id}`
    : node.dataset.build ? `[data-build="${CSS.escape(node.dataset.build)}"]`
    : node.dataset.log ? `[data-log="${CSS.escape(node.dataset.log)}"]`
    : node.dataset.share ? `[data-share="${CSS.escape(node.dataset.share)}"]`
    : node.getAttribute("aria-label") ? `[aria-label="${CSS.escape(node.getAttribute("aria-label"))}"]`
      : node.dataset.filter ? `[data-filter="${node.dataset.filter}"]`
        : null;
}

function render() {
  const key = focusKey(document.activeElement);
  renderPage();
  state.renderedAt = Date.now();
  if (key && document.activeElement === document.body) {
    const again = document.querySelector(key);
    if (again) again.focus({ preventScroll: true });
  }
}

function renderPage() {
  const builds = visibleBuilds();
  const page = state.page;
  $("page-title").textContent = TITLES[page];
  document.title = `${TITLES[page]} · Ant`;
  for (const link of document.querySelectorAll(".nav a")) {
    if (link.dataset.page === page) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
  $("nav-builds").textContent = state.builds.length ? String(state.builds.length) : "";
  $("nav-projects").textContent = state.builds.length ? String(projects(state.builds).length) : "";
  const fileCount = currentFiles(state.builds).filter(({ file }) => file.exists).length;
  $("nav-deliverables").textContent = fileCount ? String(fileCount) : "";

  renderRunning();
  const empty = state.loaded && state.builds.length === 0 && state.running.length === 0;
  $("empty").hidden = !empty;
  for (const name of Object.keys(TITLES)) $(`page-${name}`).hidden = empty || name !== page;
  if (empty) return;
  if (page === "overview") renderOverview(builds);
  if (page === "builds") renderBuilds(builds);
  if (page === "projects") renderProjects(builds);
  if (page === "deliverables") renderDeliverables(builds);
}

function route() {
  const page = location.hash.replace("#", "");
  state.page = TITLES[page] ? page : "overview";
  closeMenu();
  render();
}

async function refresh() {
  try {
    const response = await fetch("api/builds", { cache: "no-store" });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const data = await response.json();
    state.builds = data.builds;
    state.running = data.running || [];
    state.now = data.now_ms;
    state.clockOffset = data.now_ms - Date.now();
    state.lastOk = Date.now();
    state.loaded = true;
    if (state.project && !state.builds.some((b) => b.project === state.project)) state.project = null;
    $("live").classList.remove("bad");
    $("live-text").textContent = state.running.length ? `Live · ${state.running.length} building` : "Live";
    $("banner").hidden = true;
    // Only re-render when something changed, or once a minute so relative
    // times move: rebuilding the page every few seconds would steal focus.
    const signature = JSON.stringify([
      data.builds.map((b) => [b.id, b.downloads.map((f) => [f.exists, f.replaced, f.bytes])]),
      state.running.map((b) => [b.id, b.steps.length, b.current]),
    ]);
    if (signature !== state.signature || Date.now() - state.renderedAt > 60000) {
      state.signature = signature;
      render();
    }
  } catch (error) {
    $("live").classList.add("bad");
    $("live-text").textContent = "Offline";
    const banner = $("banner");
    banner.textContent = `Lost contact with Ant${state.lastOk ? ` (last update ${ago(state.lastOk, Date.now())})` : ""}. Is \`ant dashboard\` still running in your terminal?`;
    banner.hidden = false;
  }
}

// Theme: follows the system unless the viewer picked one (remembered in
// this browser only; storage can be unavailable, which is fine).
function applyTheme(theme) {
  if (theme) document.documentElement.dataset.theme = theme;
  else delete document.documentElement.dataset.theme;
}
function currentTheme() {
  return document.documentElement.dataset.theme
    || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
}
try { applyTheme(localStorage.getItem("ant-theme")); } catch { /* storage unavailable */ }
$("theme").addEventListener("click", () => {
  const next = currentTheme() === "dark" ? "light" : "dark";
  applyTheme(next);
  try { localStorage.setItem("ant-theme", next); } catch { /* storage unavailable */ }
});

function openMenu() {
  $("sidebar").classList.add("open");
  $("scrim").hidden = false;
  $("menu").setAttribute("aria-expanded", "true");
}
function closeMenu() {
  $("sidebar").classList.remove("open");
  $("scrim").hidden = true;
  $("menu").setAttribute("aria-expanded", "false");
}
$("menu").addEventListener("click", () => ($("sidebar").classList.contains("open") ? closeMenu() : openMenu()));
$("scrim").addEventListener("click", closeMenu);
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") closeMenu();
  if (event.key === "/" && document.activeElement !== $("search")) {
    event.preventDefault();
    $("search").focus();
  }
});

$("search").addEventListener("input", (event) => {
  state.query = event.target.value.trim();
  render();
});
for (const chip of document.querySelectorAll(".chip")) {
  chip.addEventListener("click", () => {
    state.filter = chip.dataset.filter;
    render();
  });
}
window.addEventListener("hashchange", route);

// Poll every 3 s, every second while something is building.
async function poll() {
  await refresh();
  followLiveLogs();
  setTimeout(poll, state.running.length ? 1000 : REFRESH_MS);
}

route();
poll();
