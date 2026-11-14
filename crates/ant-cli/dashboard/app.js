// Ant local dashboard (D-042). All data comes from this origin's api/builds
// and is rendered through DOM APIs with textContent (never innerHTML).
"use strict";

const REFRESH_MS = 3000;
const DAY = 86400000;
const SVG = "http://www.w3.org/2000/svg";

const state = {
  builds: [],
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

  const empty = state.loaded && state.builds.length === 0;
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
    state.now = data.now_ms;
    state.lastOk = Date.now();
    state.loaded = true;
    if (state.project && !state.builds.some((b) => b.project === state.project)) state.project = null;
    $("live").classList.remove("bad");
    $("live-text").textContent = "Live";
    $("banner").hidden = true;
    // Only re-render when something changed, or once a minute so relative
    // times move: rebuilding the page every few seconds would steal focus.
    const signature = JSON.stringify(data.builds.map((b) => [b.id, b.downloads.map((f) => [f.exists, f.replaced, f.bytes])]));
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

route();
refresh();
setInterval(refresh, REFRESH_MS);
