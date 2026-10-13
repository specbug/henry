"use strict";

// Glass: Henry's window. Plain script, no build step: every view is a few
// sections of HTML rendered from the API's JSON, repainted only where they
// changed so a half-typed secret survives the next poll.

const main = document.getElementById("main");
const bar = document.querySelector(".top");

// --- small tools ----------------------------------------------------------

class Raw {
  constructor(s) { this.s = s; }
}
const raw = (s) => new Raw(s);
const esc = (s) =>
  String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const out = (v) => (v == null || v === false ? "" : v instanceof Raw ? v.s : Array.isArray(v) ? v.map(out).join("") : esc(v));
// A template tag: interpolations are escaped unless already HTML.
function h(strings, ...vals) {
  let s = strings[0];
  vals.forEach((v, i) => { s += out(v) + strings[i + 1]; });
  return raw(s);
}

async function api(path, { method = "GET", body } = {}) {
  const init = { method, credentials: "same-origin", headers: {} };
  if (body !== undefined) {
    init.headers["content-type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  const res = await fetch("/api" + path, init);
  let data = null;
  try { data = await res.json(); } catch (_) { /* an empty answer */ }
  if (!res.ok) {
    const err = new Error((data && data.error) || res.statusText);
    err.status = res.status;
    throw err;
  }
  return data;
}

const utc = (ms) => new Date(ms).toISOString().replace("T", " ").slice(0, 16) + " UTC";
function ago(ms, now) {
  const s = Math.max(0, (now - ms) / 1000);
  if (s < 45) return "just now";
  const m = s / 60;
  if (m < 60) return `${Math.round(m)} min ago`;
  const hr = m / 60;
  if (hr < 24) return `${Math.round(hr)} h ago`;
  const d = hr / 24;
  if (d < 45) return `${Math.round(d)} days ago`;
  return utc(ms).slice(0, 10);
}
const when = (ms, now) => (ms ? h`<time datetime="${new Date(ms).toISOString()}" title="${utc(ms)}">${ago(ms, now)}</time>` : "");
const plural = (n, one, many) => `${n} ${n === 1 ? one : many || one + "s"}`;
const tokens = (n) => (n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${Math.round(n / 1e3)}k` : String(n));
const bytes = (n) => {
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i ? 1 : 0)} ${u[i]}`;
};
const first = (s) => (s || "").split("\n")[0];
const slug = (repo) => (repo || "").replace(/^file:\/\/.*\/([^/]+)$/, "$1").replace(/^https:\/\/github\.com\//, "").replace(/^git@github\.com:/, "").replace(/\.git$/, "");

const WORDS = {
  live: "Live",
  needs_you: "Needs you",
  degraded: "Degraded",
  down: "Down",
  stopped: "Stopped",
  onboarding: "Onboarding",
  stuck: "Stuck",
};

// --- the strip ------------------------------------------------------------

// One mark an hour for thirty days. Height says whether the app was meant to
// be serving; colour is kept for trouble, and the accent for waiting on you.
function strip(code, big) {
  const H = big ? 40 : 18;
  const n = code.length;
  // Trouble is drawn after the quiet marks, so live hours never cover it.
  let quiet = "";
  let trouble = "";
  let i = 0;
  while (i < n) {
    const c = code[i];
    let j = i;
    while (j < n && code[j] === c) j++;
    if ("ndkx".includes(c)) {
      // Visible at any width: at least three hours wide.
      const w = Math.max(j - i, 3);
      const x = Math.max(0, Math.min(n - w, i - (w - (j - i)) / 2));
      trouble += `<rect class="m-${c}" x="${x}" y="0" width="${w}" height="${H}"/>`;
    } else if (c !== ".") {
      const tall = c === "s" || c === "o" ? Math.round(H * 0.3) : H;
      quiet += `<rect class="m-${c}" x="${i}" y="${H - tall}" width="${j - i}" height="${tall}"/>`;
    }
    i = j;
  }
  // A hairline for the whole month, so hours with no mark read as time.
  const rects = `<rect class="track" x="0" y="${H - 1}" width="${n}" height="1"/>` + quiet + trouble;
  return raw(`<svg class="strip${big ? " big" : ""}" viewBox="0 0 ${n} ${H}" preserveAspectRatio="none" role="img" aria-label="${esc(describe(code))}">${rects}</svg>`);
}

function describe(code) {
  const count = (c) => code.split(c).length - 1;
  const seen = code.length - count(".");
  if (!seen) return "No history yet";
  const parts = [`${plural(count("l"), "hour")} live`];
  if (count("x") + count("k")) parts.push(`${plural(count("x") + count("k"), "hour")} down`);
  if (count("d")) parts.push(`${plural(count("d"), "hour")} degraded`);
  if (count("n")) parts.push(`${plural(count("n"), "hour")} waiting on you`);
  return parts.join(", ") + " in the last 30 days";
}

const share = (s) => (s == null ? "" : s >= 0.9995 ? "Live the whole time it was meant to be." : `Live ${(s * 100).toFixed(s > 0.99 ? 2 : 1)}% of the time it was meant to be.`);

// --- painting -------------------------------------------------------------

const painted = new Map();
let lastEvent = 0;

function busy(el) {
  const a = document.activeElement;
  return (a && el.contains(a) && a.matches("input, textarea, select")) ||
    [...el.querySelectorAll("input")].some((i) => i.value !== "");
}

// Paint `[key, html]` sections. Unchanged sections are left alone, and so
// is any section someone is typing in.
function paint(sections) {
  const keys = sections.map(([k]) => k).join("|");
  if (main.dataset.keys !== keys) {
    main.innerHTML = sections.map(([k, v]) => `<section data-k="${k}">${out(v)}</section>`).join("");
    main.dataset.keys = keys;
    painted.clear();
    sections.forEach(([k, v]) => painted.set(k, out(v)));
    main.classList.add("enter");
    setTimeout(() => main.classList.remove("enter"), 600);
    return;
  }
  for (const [k, v] of sections) {
    const html = out(v);
    if (painted.get(k) === html) continue;
    const el = main.querySelector(`section[data-k="${k}"]`);
    if (!el || busy(el)) continue;
    el.innerHTML = html;
    painted.set(k, html);
  }
}

function fresh(view) {
  main.dataset.keys = "";
  main.innerHTML = "";
  painted.clear();
  lastEvent = 0;
  document.querySelectorAll("[data-nav]").forEach((a) => {
    if (a.dataset.nav === view) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  });
}

// A status change reads as the UI says it: "Needs you: …", not "needs_you: …".
function said(e) {
  if (e.kind !== "app.status") return e.message;
  const [word, ...rest] = e.message.split(": ");
  return (WORDS[word] || word) + (rest.length ? ": " + rest.join(": ") : "");
}

function events(list, now) {
  if (!list.length) return h`<p class="quiet">Nothing has happened yet.</p>`;
  const newest = list.reduce((m, e) => Math.max(m, e.id), 0);
  const html = h`<ul class="events">${list.map((e) => h`<li class="${lastEvent && e.id > lastEvent ? "new" : ""}">${when(e.at, now)}<span>${e.app && e.app !== route.name ? h`<span class="faint">${e.app} · </span>` : ""}${said(e)}</span></li>`)}</ul>`;
  lastEvent = Math.max(lastEvent, newest);
  return html;
}

// --- views ----------------------------------------------------------------

async function viewApps() {
  const d = await api("/apps");
  const now = d.now;
  const all = d.apps.length + d.drafts.length;
  if (!all) {
    return paint([["lede", h`<p class="lede">Nothing here yet. <a href="#/add">Add a repo</a> and Henry takes it from there: it reads the code, writes a plan, builds it, and proves it works before anything is public.</p>`]]);
  }
  const by = (s) => d.apps.filter((a) => a.status === s).length;
  const parts = [];
  if (by("needs_you")) parts.push(h`<span class="status needs_you">${by("needs_you")} waiting on you</span>`);
  const broken = by("down") + by("stuck");
  if (broken) parts.push(h`<span class="status down">${broken} down</span>`);
  if (by("degraded")) parts.push(`${by("degraded")} degraded`);
  const onboarding = by("onboarding") + d.drafts.length;
  if (onboarding) parts.push(`${onboarding} onboarding`);
  const live = by("live");
  let lede;
  if (live === all) lede = all === 1 ? "One app, live." : `${all} apps, all live.`;
  else lede = h`${plural(all, "app")}. ${live ? `${live} live` : "None live yet"}${parts.length ? raw(", " + parts.map(out).join(", ")) : ""}.`;

  const rows = d.apps.map((a) => {
    const note = a.status === "needs_you" && a.missing.length
      ? `waiting on ${a.missing.join(", ")}`
      : a.onboarding ? `${a.onboarding.stage}${a.onboarding.summary ? " — " + a.onboarding.summary : ""}` : first(a.detail);
    return h`<li><a class="row" href="#/app/${encodeURIComponent(a.name)}">
      <span><span class="name">${a.name}</span><span class="sub meta">${slug(a.repo)}</span></span>
      <span><span class="status ${a.status}">${WORDS[a.status] || a.status}</span>${note ? h`<span class="note">${note}</span>` : ""}</span>
      <span class="strip-cell">${strip(a.strip)}</span>
      <span class="when meta">${a.live ? h`<span class="mono">${a.live.short}</span><br>` : ""}${when(a.updated_at, now)}</span>
    </a></li>`;
  });
  const drafts = d.drafts.map((x) => {
    const o = x.onboarding;
    const status = o.stage === "stuck" ? "stuck" : "onboarding";
    return h`<li><a class="row" href="#/app/${encodeURIComponent(x.name)}">
      <span><span class="name">${x.name}</span><span class="sub meta">${slug(o.repo)}</span></span>
      <span><span class="status ${status}">${WORDS[status]}</span><span class="note">${o.stage}${o.summary ? " — " + o.summary : ""}</span></span>
      <span class="strip-cell"></span>
      <span class="when meta">${when(o.updated_at, now)}</span>
    </a></li>`;
  });
  paint([["lede", h`<p class="lede">${lede}</p>`], ["rows", h`<ul class="rows">${rows}${drafts}</ul>`]]);
}

const STAGES = [
  ["survey", "Read the repo"],
  ["plan", "Write a plan"],
  ["build", "Build it"],
  ["trial", "Prove it"],
  ["live", "Live"],
];

function onboarding(o, now) {
  const at = STAGES.findIndex(([s]) => s === o.stage);
  const steps = STAGES.map(([s, label], i) => {
    const cls = o.stage === "stuck" ? "" : i < at ? "done" : i === at ? "now" : "";
    return h`<li class="${cls}">${label}</li>`;
  });
  const stuck = o.stage === "stuck";
  return h`<h2>Onboarding</h2>
    <ol class="stages">${steps}</ol>
    ${stuck ? h`<p class="error">Stuck: ${first(o.detail)}</p>` : o.summary ? h`<p class="quiet">${o.summary}</p>` : ""}
    <p class="meta">round ${o.round} · ${tokens(o.spent)} model tokens · updated ${ago(o.updated_at, now)}</p>
    ${stuck ? h`<button data-act="retry" data-repo="${o.repo}" data-branch="${o.branch || ""}" data-hostname="${o.hostname || ""}">Try again</button>` : ""}`;
}

function plans(list, now) {
  if (!list.length) return "";
  return h`<h2>Plans</h2>${list.map((p) => h`<details>
    <summary><span>Plan ${p.id}</span><span class="meta">round ${p.round} · ${p.source}${p.model ? " · " + p.model : ""}</span><span class="${p.verdict === "accepted" ? "ok" : p.verdict ? "bad" : "quiet"}">${p.verdict ? first(p.verdict) : "being tried"}</span></summary>
    ${p.notes ? h`<p class="quiet">${p.notes}</p>` : ""}
    ${p.verdict && p.verdict.includes("\n") ? h`<pre>${p.verdict}</pre>` : ""}
    <pre>${p.lease}</pre>
    ${Object.entries(p.files || {}).map(([path, text]) => h`<p class="meta">${path}</p><pre>${text}</pre>`)}
    ${p.generate && p.generate.length ? h`<p class="meta">generated secrets: ${p.generate.join(", ")}</p>` : ""}
    <p class="meta">${when(p.created_at, now)}</p>
  </details>`)}`;
}

async function viewApp(name) {
  const d = await api("/apps/" + encodeURIComponent(name));
  const now = d.now;
  if (!d.app) {
    const o = d.onboarding;
    return paint([
      ["head", h`<h1>${name}</h1><p class="where meta"><a href="${o.repo.startsWith("https://") ? o.repo : "#"}">${slug(o.repo)}</a>${o.branch ? " · " + o.branch : ""}${o.hostname ? " · " + o.hostname : ""}</p>`],
      ["onboarding", onboarding(o, now)],
      ["plans", plans(d.plans, now)],
      ["events", h`<h2>What happened</h2>${events(d.events, now)}`],
    ]);
  }
  const running = d.desired === "running";
  const head = h`<div class="head">
      <div>
        <h1>${d.name}</h1>
        <p class="where meta"><a href="${d.repo.startsWith("https://") ? d.repo : "#"}">${slug(d.repo)}</a> · ${d.branch || ""}${d.auto_deploy ? "" : " · manual deploys"} · lease by ${d.source}${d.model ? " · " + d.model : ""}</p>
      </div>
      <div class="actions">
        <button class="primary" data-act="deploy">Deploy</button>
        ${running ? h`<button data-act="stop">Stop</button>` : h`<button data-act="start">Start</button>`}
        ${d.backups && d.volumes.length ? h`<button data-act="backup">Back up</button>` : ""}
      </div>
    </div>
    <p class="lede"><span class="status ${d.status}">${WORDS[d.status] || d.status}</span>${d.detail ? h`<span class="quiet"> — ${first(d.detail)}</span>` : ""}${d.live ? h`<span class="quiet">. Release ${d.live.id}, <span class="mono">${d.live.short}</span>.</span>` : ""}</p>`;

  const missing = d.secrets.missing;
  const waiting = missing.length
    ? h`<div class="waiting">
        <h3>Waiting on ${plural(missing.length, "secret")}</h3>
        <p>Henry holds the release until ${missing.length === 1 ? "it is" : "they are"} set. A value is sealed on the Mac and never shown again, here or anywhere.</p>
        ${missing.map((m) => h`<form data-form="secret" data-name="${m}"><label class="mono" for="s-${m}">${m}</label><input id="s-${m}" type="password" autocomplete="off" spellcheck="false" required><button class="primary">Seal</button></form>`)}
      </div>`
    : d.status === "needs_you" ? h`<div class="waiting"><h3>Waiting on you</h3><p>${d.detail || ""}</p></div>` : "";

  const health = h`<h2>Thirty days</h2>${strip(d.strip, true)}<div class="scale meta"><span>30 days ago</span><span>15</span><span>now</span></div><p class="quiet after">${share(d.share) || "No hours served yet."}</p>`;

  let trial = "";
  if (d.trial) {
    const r = d.trial.release;
    const p = d.trial.proof;
    trial = h`<h2>On trial</h2><p>Release ${r.id}, <span class="mono">${r.short}</span>, ${d.live ? "is running beside the live one" : "is being tried; nothing is live yet"}. ${p ? h`Stage: ${String(p.stage).toLowerCase()}; ${plural(p.probes, "probe")}, ${p.failed_probes} failed${p.last_failure ? h`. Last failure: <span class="quiet">${p.last_failure}</span>` : ""}.` : "Proof has not looked at it yet."}</p>`;
  }

  const ob = d.onboarding_record && d.onboarding_record.stage !== "live" ? onboarding(d.onboarding_record, now) : "";

  const releases = d.releases.length
    ? h`<h2>Releases</h2><table><thead><tr><th>#</th><th>Commit</th><th>State</th><th class="wide-only">What happened</th><th class="num">When</th></tr></thead><tbody>${d.releases.map((r) => h`<tr>
        <td class="mono">${r.id}</td><td><span class="mono">${r.short}</span></td>
        <td class="${r.state === "failed" ? "bad" : r.state === "live" ? "" : "quiet"}">${r.state}</td>
        <td class="wide-only quiet">${r.summary || ""}${r.reason && r.reason.includes("\n") ? h` <details><summary class="meta">the log's end</summary><pre>${r.reason}</pre></details>` : ""}</td>
        <td class="num meta">${when(r.created_at, now)}</td></tr>`)}</tbody></table>`
    : "";

  const services = h`<h2>Services</h2><table><thead><tr><th>Service</th><th>Public at</th><th class="wide-only">Volumes</th><th class="wide-only">Secrets</th></tr></thead><tbody>${d.services.map((s) => h`<tr>
      <td>${s.name}</td><td>${s.public ? h`<a href="${s.public}">${s.public.replace("https://", "")}</a>` : h`<span class="faint">private</span>`}</td>
      <td class="wide-only mono">${s.volumes.join(", ")}</td>
      <td class="wide-only mono">${s.secrets.map((n) => h`<span class="${missing.includes(n) ? "status needs_you" : ""}">${n}</span> `)}</td></tr>`)}</tbody></table>`;

  const volumes = d.volumes.length
    ? h`<h2>Backups</h2><table><thead><tr><th>Volume</th><th>Last backup</th><th>Restore drill</th></tr></thead><tbody>${d.volumes.map((v) => h`<tr>
        <td class="mono">${v.target} <span class="faint">${v.kind}</span></td>
        <td>${!v.backup ? h`<span class="quiet">never</span>` : v.backup.ok ? h`${when(v.backup.at, now)}${v.backup.bytes ? h` <span class="faint">· ${bytes(v.backup.bytes)}</span>` : ""}` : h`<span class="bad">failed ${ago(v.backup.at, now)}</span> <span class="quiet">${first(v.backup.detail)}</span>`}</td>
        <td>${v.proven ? (v.drill && !v.drill.ok ? h`<span class="bad">last drill failed</span> <span class="quiet">${first(v.drill.detail)}</span>` : h`restored ${when(v.drill.at, now)}`) : h`<span class="quiet">not yet proven restorable</span>`}</td></tr>`)}</tbody></table>`
    : "";

  paint([
    ["head", head],
    ["waiting", waiting],
    ["health", health],
    ["onboarding", ob],
    ["trial", trial],
    ["releases", releases],
    ["services", services],
    ["volumes", volumes],
    ["logs", h`<h2>Logs</h2><div class="log" id="logs"><button data-act="logs">Show the last 200 lines</button></div>`],
    ["plans", plans(d.plans, now)],
    ["events", h`<h2>What happened</h2>${events(d.events, now)}`],
  ]);
}

async function showLogs(name) {
  const box = document.getElementById("logs");
  box.innerHTML = `<p class="quiet">Reading…</p>`;
  try {
    const d = await api(`/apps/${encodeURIComponent(name)}/logs?lines=200`);
    box.innerHTML = out(d.logs.length
      ? h`${d.logs.map((l) => h`<p class="meta">${l.service} · ${l.release ? "release " + l.release : "stateful"} · ${l.container}</p><pre>${l.text || "(nothing yet)"}</pre>`)}<button data-act="logs">Read again</button>`
      : h`<p class="quiet">No containers are running for it.</p><button data-act="logs">Read again</button>`);
    box.querySelectorAll("pre").forEach((p) => { p.scrollTop = p.scrollHeight; });
  } catch (e) {
    box.innerHTML = out(h`<p class="error">${e.message}</p><button data-act="logs">Try again</button>`);
  }
}

function viewAdd() {
  paint([["add", h`<h1>Add a repo</h1>
    <p class="lede">Henry reads the repo, writes a plan for running it, builds it, and proves it works. Nothing goes public until it has.</p>
    <form class="form" data-form="onboard">
      <label><span>Repository</span><input name="repo" placeholder="https://github.com/you/app" required autocomplete="off" spellcheck="false"></label>
      <label><span>Name</span><input name="name" placeholder="from the repo's name" autocomplete="off" spellcheck="false"><small>Lowercase letters, digits and dashes.</small></label>
      <label><span>Public hostname</span><input name="hostname" placeholder="app.example.com" autocomplete="off" spellcheck="false"><small>Leave it empty to keep the app private.</small></label>
      <label><span>Branch</span><input name="branch" placeholder="the repo's default" autocomplete="off" spellcheck="false"></label>
      <p class="error" id="add-error" hidden></p>
      <div><button class="primary">Begin onboarding</button></div>
    </form>`]]);
}

async function viewHouse() {
  const d = await api("/house");
  const now = d.now;
  const checks = d.checks.length
    ? h`<table><tbody>${d.checks.map((c) => h`<tr><td>${c.name}</td><td class="quiet">${c.want}</td><td class="mono ${c.ok === false ? "bad" : ""}">${c.got}</td><td class="num">${c.ok === true ? "ok" : c.ok === false ? h`<span class="bad">drift</span>` : h`<span class="faint">unknown</span>`}</td></tr>`)}</tbody></table>
       <p class="quiet after">Henry reads these settings and reports drift; changing them is yours.</p>`
    : h`<p class="quiet">The host checks read macOS settings, and this is not a Mac.</p>`;
  const every = (hrs) => (hrs % 168 === 0 ? (hrs === 168 ? "weekly" : `every ${hrs / 168} weeks`) : hrs % 24 === 0 ? (hrs === 24 ? "daily" : `every ${hrs / 24} days`) : `every ${hrs} h`);
  const upkeep = h`<table><thead><tr><th>Task</th><th>Runs</th><th>Last</th><th class="wide-only">What it found</th><th></th></tr></thead><tbody>${d.upkeep.map((u) => h`<tr>
      <td>${u.task}</td><td class="quiet">${u.on ? every(u.every_hours) : "off"}</td>
      <td>${u.last ? (u.last.ok ? when(u.last.at, now) : h`<span class="bad">failed ${ago(u.last.at, now)}</span>`) : h`<span class="faint">never</span>`}</td>
      <td class="wide-only quiet">${u.last ? first(u.last.detail) : ""}</td>
      <td class="num"><button class="link" data-act="upkeep" data-task="${u.task}">Run now</button></td></tr>`)}</tbody></table>`;
  const tools = d.tools.length
    ? h`<table><thead><tr><th>Tool</th><th>Version</th><th class="wide-only">Falls back to</th><th class="num">Since</th></tr></thead><tbody>${d.tools.map((t) => h`<tr><td>${t.name}</td><td class="mono">${t.current}${t.pending ? h` <span class="faint">→ ${t.pending}</span>` : ""}</td><td class="wide-only mono quiet">${t.previous || ""}</td><td class="num meta">${when(t.since, now)}</td></tr>`)}</tbody></table>`
    : h`<p class="quiet">Henry owns no tools yet. <code>henry tools adopt caddy</code> hands one over.</p>`;
  const bench = d.bench.length
    ? h`<table><thead><tr><th>Model</th><th>Onboarded</th><th class="num">Tokens</th><th class="num">When</th></tr></thead><tbody>${d.bench.map((b) => h`<tr><td class="mono">${b.model}</td><td>${b.passed} of ${b.total}</td><td class="num mono">${tokens(b.tokens)}</td><td class="num meta">${when(b.at, now)}</td></tr>`)}</tbody></table>`
    : h`<p class="quiet">No model has been scored yet.</p>`;
  paint([
    ["lede", h`<h1>The house</h1><p class="lede">What keeps Henry and this Mac in order.</p>`],
    ["checks", h`<h2>This Mac</h2>${checks}`],
    ["upkeep", h`<h2>Upkeep</h2>${upkeep}`],
    ["tools", h`<h2>Tools</h2>${tools}`],
    ["bench", h`<h2>Models</h2>${bench}`],
    ["events", h`<h2>Henry</h2>${events(d.events, now)}`],
  ]);
}

function locked(message) {
  bar.hidden = true;
  main.dataset.keys = "";
  main.innerHTML = out(h`<div class="locked">
    <h1>Glass is locked</h1>
    <p class="lede">On the Mac, run <code>henry open</code> to sign in there. From anywhere else, come in through Cloudflare Access at Glass's own address.</p>
    <p class="faint mono">${message}</p>
  </div>`);
}

// --- routing and polling --------------------------------------------------

const route = { view: "apps", name: null };
let timer = null;
let inflight = false;

function parse() {
  const parts = location.hash.replace(/^#\/?/, "").split("/");
  if (parts[0] === "app" && parts[1]) return { view: "app", name: decodeURIComponent(parts[1]) };
  if (parts[0] === "add") return { view: "add", name: null };
  if (parts[0] === "house") return { view: "house", name: null };
  return { view: "apps", name: null };
}

async function refresh() {
  if (inflight || document.hidden) return;
  inflight = true;
  try {
    if (route.view === "apps") await viewApps();
    else if (route.view === "app") await viewApp(route.name);
    else if (route.view === "house") await viewHouse();
  } catch (e) {
    if (e.status === 401 || e.status === 403) locked(e.message);
    else if (e.status === 404) paint([["missing", h`<p class="lede">There is no app named ${route.name}. <a href="#/">Back to the apps</a>.</p>`]]);
    else paint([["error", h`<p class="error">${e.message}</p>`]]);
  } finally {
    inflight = false;
  }
}

function go() {
  Object.assign(route, parse());
  fresh(route.view === "app" ? "apps" : route.view);
  clearInterval(timer);
  if (route.view === "add") return viewAdd();
  refresh();
  timer = setInterval(refresh, route.view === "house" ? 30000 : 4000);
}

document.addEventListener("click", async (ev) => {
  const b = ev.target.closest("[data-act]");
  if (!b) return;
  const act = b.dataset.act;
  const name = route.name;
  const path = `/apps/${encodeURIComponent(name)}`;
  try {
    if (act === "logs") return showLogs(name);
    if (act === "stop" && !confirm(`Stop ${name}? It stays stopped until you start it again.`)) return;
    b.disabled = true;
    if (act === "deploy" || act === "stop" || act === "start" || act === "backup") {
      await api(`${path}/${act}`, { method: "POST", body: {} });
    } else if (act === "upkeep") {
      await api(`/house/${b.dataset.task}`, { method: "POST", body: {} });
      b.textContent = "Queued";
      return;
    } else if (act === "retry") {
      await api("/onboard", { method: "POST", body: { repo: b.dataset.repo, name, branch: b.dataset.branch, hostname: b.dataset.hostname } });
    }
    await refresh();
  } catch (e) {
    alert(e.message);
  } finally {
    b.disabled = false;
  }
});

document.addEventListener("submit", async (ev) => {
  const f = ev.target.closest("[data-form]");
  if (!f) return;
  ev.preventDefault();
  const button = f.querySelector("button");
  button.disabled = true;
  try {
    if (f.dataset.form === "secret") {
      const input = f.querySelector("input");
      await api(`/apps/${encodeURIComponent(route.name)}/secrets/${encodeURIComponent(f.dataset.name)}`, { method: "PUT", body: { value: input.value } });
      input.value = "";
      input.blur();
      await refresh();
    } else if (f.dataset.form === "onboard") {
      const v = Object.fromEntries(new FormData(f));
      const d = await api("/onboard", { method: "POST", body: v });
      location.hash = `#/app/${encodeURIComponent(d.name)}`;
    }
  } catch (e) {
    const err = f.querySelector(".error");
    if (err) { err.textContent = e.message; err.hidden = false; } else alert(e.message);
  } finally {
    button.disabled = false;
  }
});

document.addEventListener("visibilitychange", () => { if (!document.hidden) refresh(); });
window.addEventListener("hashchange", go);

(async () => {
  try {
    const s = await api("/session");
    bar.hidden = false;
    bar.querySelector(".who").textContent = s.who;
    go();
  } catch (e) {
    locked(e.message);
  }
})();
