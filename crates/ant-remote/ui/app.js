// Ant coordinator dashboard. Reads only the coordinator's own read-model API
// (GET /v1/jobs, GET /v1/workers) with a viewer or client token. All data is
// rendered with textContent (never innerHTML): job and action ids come from
// clients and must never be interpreted as markup.
"use strict";

const TOKEN_KEY = "ant.coordinator.token";
const REFRESH_MS = 3000;

const $ = (id) => document.getElementById(id);
let timer = null;
let paused = false;

function storedToken() {
  try { return sessionStorage.getItem(TOKEN_KEY); } catch { return null; }
}
function storeToken(value) {
  try {
    if (value) sessionStorage.setItem(TOKEN_KEY, value); else sessionStorage.removeItem(TOKEN_KEY);
  } catch { /* storage unavailable: token lives only in memory for this page */ }
}
let memoryToken = storedToken();

function setStatus(text, bad) {
  const el = $("status");
  el.textContent = text;
  el.classList.toggle("bad", Boolean(bad));
}

function el(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined && text !== null) node.textContent = String(text);
  if (className) node.className = className;
  return node;
}

function fmtTime(ms) {
  if (!ms) return "—";
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}
function fmtDuration(ms) {
  if (ms === null || ms === undefined || ms < 0) return "—";
  if (ms < 1000) return `${ms} ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(1)} s`;
  const m = Math.floor(s / 60);
  return `${m} min ${Math.round(s - m * 60)} s`;
}
function fmtAgo(ms, now) {
  const delta = Math.max(0, now - ms);
  if (delta < 2000) return "just now";
  return `${fmtDuration(delta)} ago`;
}

/** The label shown for a job: derived only from what the coordinator reports. */
function jobLabel(job) {
  if (job.state === "queued") return "queued";
  if (job.state === "running") return "running";
  if (job.state === "completed") return job.succeeded ? "succeeded" : "failed";
  return "failed";
}

async function api(path) {
  const response = await fetch(path, {
    headers: { Authorization: `Bearer ${memoryToken}` },
    cache: "no-store",
  });
  if (response.status === 401 || response.status === 403) {
    const error = new Error(response.status === 401 ? "Token rejected by the coordinator." : "This token may not read the dashboard.");
    error.auth = true;
    throw error;
  }
  if (!response.ok) throw new Error(`Coordinator returned HTTP ${response.status}.`);
  return response.json();
}

function renderJobs(list) {
  const body = $("jobs");
  body.replaceChildren();
  const counts = { queued: 0, running: 0, succeeded: 0, failed: 0 };
  const now = Date.now();
  for (const job of list.jobs) {
    const label = jobLabel(job);
    counts[label] += 1;
    const tr = document.createElement("tr");
    const id = el("td"); id.append(el("code", job.job_id)); tr.append(id);
    const action = el("td"); action.append(el("code", job.action_id)); tr.append(action);
    const state = el("td"); state.append(el("span", label, `badge ${label}`)); tr.append(state);
    tr.append(el("td", job.worker_id || "—", "nowrap"));
    tr.append(el("td", job.attempts, "num"));
    const submitted = el("td", fmtTime(job.submitted_unix_ms), "nowrap");
    submitted.title = new Date(job.submitted_unix_ms).toISOString();
    tr.append(submitted);
    let duration = null;
    if (job.started_unix_ms) duration = (job.finished_unix_ms || now) - job.started_unix_ms;
    const dur = el("td", fmtDuration(duration), "num");
    if (job.state === "running") dur.append(el("span", "so far", "detail"));
    tr.append(dur);
    const result = el("td");
    if (job.exit_code !== null && job.exit_code !== undefined) result.append(el("span", `exit ${job.exit_code}`));
    else if (job.state === "completed" || job.state === "failed") result.append(el("span", "no exit code"));
    else result.append(el("span", "—"));
    if (job.error) result.append(el("span", job.error, "detail"));
    if (job.output_count) result.append(el("span", `${job.output_count} output entr${job.output_count === 1 ? "y" : "ies"}`, "detail"));
    tr.append(result);
    body.append(tr);
  }
  for (const key of Object.keys(counts)) $(`n-${key}`).textContent = counts[key];
  const empty = $("jobs-empty");
  empty.hidden = list.jobs.length > 0;
  body.closest(".table-wrap").hidden = list.jobs.length === 0;
  empty.textContent = `No jobs since the coordinator started at ${fmtTime(list.coordinator_started_unix_ms)}. Run "ant remote run" to submit a plan.`;
  $("jobs-note").textContent =
    `Jobs held in coordinator memory since ${new Date(list.coordinator_started_unix_ms).toLocaleString()}; a coordinator restart clears this list.`;
}

function renderWorkers(list) {
  const body = $("workers");
  body.replaceChildren();
  for (const worker of list.workers) {
    const tr = document.createElement("tr");
    const id = el("td"); id.append(el("code", worker.worker_id)); tr.append(id);
    tr.append(el("td", worker.os));
    const seen = el("td", fmtAgo(worker.last_seen_unix_ms, list.now_unix_ms), "nowrap");
    seen.title = new Date(worker.last_seen_unix_ms).toISOString();
    tr.append(seen);
    const current = el("td");
    if (worker.current_job) current.append(el("code", worker.current_job)); else current.textContent = "idle";
    tr.append(current);
    tr.append(el("td", worker.jobs_reported, "num"));
    body.append(tr);
  }
  $("n-workers").textContent = list.workers.length;
  const empty = $("workers-empty");
  empty.hidden = list.workers.length > 0;
  body.closest(".table-wrap").hidden = list.workers.length === 0;
  empty.textContent = "No worker has contacted this coordinator yet. Start one with \"ant worker\".";
}

async function refresh() {
  if (!memoryToken) return;
  try {
    const [jobs, workers] = await Promise.all([api("/v1/jobs"), api("/v1/workers")]);
    renderJobs(jobs);
    renderWorkers(workers);
    showDashboard(true);
    setStatus(`${paused ? "Paused" : "Live"} · updated ${fmtTime(Date.now())}`);
  } catch (error) {
    if (error.auth) {
      disconnect(error.message);
      return;
    }
    setStatus(`Coordinator unreachable (${error.message}) · retrying`, true);
  }
}

function showDashboard(on) {
  $("dashboard").hidden = !on;
  $("connect").hidden = on;
  $("pause").hidden = !on;
}

function schedule() {
  clearInterval(timer);
  timer = paused ? null : setInterval(refresh, REFRESH_MS);
}

function disconnect(message) {
  memoryToken = null;
  storeToken(null);
  clearInterval(timer);
  showDashboard(false);
  setStatus("Not connected");
  const error = $("token-error");
  error.hidden = !message;
  error.textContent = message || "";
  if (message) $("token").focus();
}

document.addEventListener("DOMContentLoaded", () => {
  $("origin").textContent = window.location.host;
  $("token-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const value = $("token").value.trim();
    if (!value) return;
    memoryToken = value;
    storeToken(value);
    $("token").value = "";
    $("token-error").hidden = true;
    setStatus("Connecting…");
    refresh().then(schedule);
  });
  $("pause").addEventListener("click", () => {
    paused = !paused;
    $("pause").textContent = paused ? "Resume updates" : "Pause updates";
    $("pause").setAttribute("aria-pressed", String(paused));
    schedule();
    refresh();
  });
  $("forget").addEventListener("click", () => disconnect(null));
  if (memoryToken) {
    setStatus("Connecting…");
    refresh().then(schedule);
  }
});
