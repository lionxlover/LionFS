/* ==========================================================================
   LionFS — js/components.js
   Pure render functions: data.js + utils.js in, HTML strings out.
   app.js mounts these into #main / #navRoot / #siteFooter and wires
   interaction. No DOM reads here — keep it a deterministic template layer.
   ========================================================================== */

import {
  CONFIG, NAV, HERO, TICKER, HONESTY, PILLARS, CAPABILITIES, ARCHITECTURE,
  WRITE_PATH, PERFORMANCE, COMPARISON, RELEASES, GUARDIAN, TOOLS, QUICKSTART,
  DOCS, CTA, FOOTER, gh,
} from "./data.js";
import { esc, fmt, icon, highlightBash, LION_LOGO } from "./utils.js";

/* ---------- helpers ---------- */

const sectionShell = (id, head, inner, cls = "") => `
  <section id="${id}" class="section ${cls}">
    <div class="container">
      ${head ? `
      <div class="section-head${head.center ? " center" : ""}" data-reveal>
        <span class="section-eyebrow">${head.eyebrow}</span>
        <h2 class="section-title">${head.title}</h2>
        ${head.lead ? `<p class="section-lead">${head.lead}</p>` : ""}
      </div>` : ""}
      ${inner}
    </div>
  </section>`;

const reveal = (html, d = 0, kind = "") =>
  `<div data-reveal="${kind}" style="--d:${d}ms">${html}</div>`;

/* ---------- Navigation ---------- */

export function renderNav() {
  const links = NAV.map((n) => `<a href="#${n.id}" data-nav="${n.id}">${esc(n.label)}</a>`).join("");
  return `
  <a class="brand" href="#overview" aria-label="LionFS home">
    <span class="brand-mark">${LION_LOGO}</span>
    <span class="brand-name">Lion<em>FS</em></span>
    <span class="brand-ver">v${CONFIG.versionShort}</span>
  </a>
  <nav class="nav-links" aria-label="Primary">${links}</nav>
  <div class="nav-actions">
    <button class="btn-icon theme-toggle" id="themeToggle" aria-label="Toggle color theme" title="Toggle theme">
      <svg class="icon-moon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 13A9 9 0 1 1 11 3a7 7 0 0 0 10 10z"/></svg>
      <svg class="icon-sun" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="4.5"/><path d="M12 2v2.5"/><path d="M12 19.5V22"/><path d="m4.9 4.9 1.8 1.8"/><path d="m17.3 17.3 1.8 1.8"/><path d="M2 12h2.5"/><path d="M19.5 12H22"/><path d="m4.9 19.1 1.8-1.8"/><path d="m17.3 6.7 1.8-1.8"/></svg>
    </button>
    <a class="nav-gh" href="${gh()}" target="_blank" rel="noopener noreferrer" aria-label="LionFS on GitHub">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 19c-5 1.5-5-2.5-7-3m14 6v-3.9a3.4 3.4 0 0 0-1-2.6c3-.3 6.2-1.5 6.2-6.7a5.2 5.2 0 0 0-1.4-3.6 4.9 4.9 0 0 0-.1-3.7s-1.2-.4-3.9 1.5a13.4 13.4 0 0 0-7 0C6.1 1.7 4.9 2.1 4.9 2.1a4.9 4.9 0 0 0-.1 3.7A5.2 5.2 0 0 0 3.4 9.4c0 5.2 3.2 6.4 6.2 6.7a3.4 3.4 0 0 0-1 2.6V22"/><path d="M12 8v8"/><path d="M8.5 11.5h7"/></svg>
      <span class="gh-label">GitHub</span>
    </a>
    <button class="hamburger" id="hamburger" aria-expanded="false" aria-controls="navDrawer" aria-label="Open menu">
      <span></span><span></span><span></span>
    </button>
  </div>`;
}

export function renderDrawer() {
  const links = NAV.map(
    (n) => `<a href="#${n.id}" data-nav="${n.id}">${esc(n.label)}<span class="k">0${NAV.indexOf(n) + 1}</span></a>`
  ).join("");
  return `
  <div class="drawer-backdrop" id="drawerBackdrop"></div>
  <aside class="nav-drawer" id="navDrawer" aria-label="Mobile menu">
    <div class="nav-drawer-head">
      <a class="brand" href="#overview">
        <span class="brand-mark">${LION_LOGO}</span>
        <span class="brand-name">Lion<em>FS</em></span>
      </a>
      <button class="btn-icon" id="drawerClose" aria-label="Close menu">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><path d="M6 6l12 12"/><path d="M18 6 6 18"/></svg>
      </button>
    </div>
    <nav>${links}</nav>
    <div class="nav-drawer-foot">
      <a class="btn btn-primary" href="${gh()}" target="_blank" rel="noopener noreferrer">
        ${icon("github")} View on GitHub
      </a>
      <a class="btn btn-ghost" href="#quickstart">${icon("terminal")} Quick start</a>
    </div>
  </aside>`;
}

/* ---------- Hero ---------- */

export function renderHero() {
  const stats = HERO.stats
    .map(
      (s, i) => `
    <div class="stat" data-reveal style="--d:${140 + i * 90}ms">
      <span class="v"><span data-count="${s.value}">0</span><span class="u">${esc(s.unit)}</span></span>
      <span class="l">${esc(s.label)}</span>
    </div>`
    )
    .join("");

  const t = HERO.terminal;
  const terminal = `
  <div class="terminal-wrap" data-reveal="right" style="--d:150ms">
    <div class="terminal">
      <div class="term-head">
        <div class="term-dots"><span></span><span></span><span></span></div>
        <span class="term-title">${esc(t.title)}</span>
        <button class="term-replay" id="termReplay" aria-label="Replay terminal">
          ${icon("refresh")} replay
        </button>
      </div>
      <div class="term-body" id="termBody" aria-label="Demo terminal" role="img"></div>
    </div>
    <div class="term-chip">${icon("checkCircle")} snapshot create <b>O(1) · 31 µs</b></div>
  </div>`;

  const blocks = `
  <div class="hero-blocks" aria-hidden="true">
    <span class="fblock fb1" data-l="ino"></span>
    <span class="fblock fb2" data-l="B-ε"></span>
    <span class="fblock fb3" data-l="CoW"></span>
    <span class="fblock fb4" data-l="WAL"></span>
  </div>`;

  return `
  <div class="page-bg" aria-hidden="true"></div>
  <section id="overview" class="hero" aria-label="LionFS overview">
    <div class="hero-orbs" aria-hidden="true"><span class="orb orb-a"></span><span class="orb orb-b"></span></div>
    ${blocks}
    <div class="container">
      <div class="hero-grid">
        <div>
          <span class="hero-badge" data-reveal style="--d:40ms"><span class="dot"></span>${esc(HERO.badge)}</span>
          <h1 data-reveal style="--d:90ms">${esc(HERO.titleA)}<br><span class="grad-text">${esc(HERO.titleB)}</span></h1>
          <p class="hero-lead" data-reveal style="--d:160ms">${HERO.lead}</p>
          <div class="hero-ctas" data-reveal style="--d:230ms">
            <a class="btn btn-primary" href="#quickstart">${icon("terminal")} Get started ${icon("arrowRight")}</a>
            <a class="btn btn-ghost" href="#performance">${icon("chart")} View benchmarks</a>
          </div>
          <div class="hero-stats">${stats}</div>
        </div>
        ${terminal}
      </div>
    </div>
  </section>`;
}

export function renderTicker() {
  const items = [...TICKER, ...TICKER]
    .map((k) => `<span class="ticker-item">${esc(k)}<i></i></span>`)
    .join("");
  return `<div class="ticker" aria-hidden="true"><div class="ticker-track">${items}</div></div>`;
}

export function renderHonesty() {
  const chips = HONESTY.chips
    .map((c) => `<span class="chip ${c.cls}">${c.cls === "good" ? icon("checkCircle") : c.cls === "warn" ? icon("info") : icon("terminal")}${esc(c.label)}</span>`)
    .join("");
  return sectionShell(
    "honesty",
    null,
    `<div class="honesty" data-reveal>
      <div class="honesty-icon">${icon("shield")}</div>
      <div>
        <h3>${esc(HONESTY.title)}</h3>
        <p>${HONESTY.text}</p>
      </div>
      <div class="honesty-chips">${chips}</div>
    </div>`,
    "slim"
  );
}

/* ---------- Pillars ---------- */

export function renderPillars() {
  const cards = PILLARS.map((p, i) => {
    const points = p.points.map((pt) => `<li>${pt}</li>`).join("");
    return reveal(
      `
    <article class="pillar">
      <div class="pillar-top">
        <span class="pillar-icon">${icon(p.icon)}</span>
        <span class="pillar-num">${esc(p.numeral)}</span>
      </div>
      <div>
        <h3>${esc(p.title)}</h3>
        <p class="tagline">${p.tagline}</p>
      </div>
      <ul>${points}</ul>
      <div class="where">where&nbsp; ${p.where}</div>
    </article>`,
      i * 90
    );
  }).join("");

  return sectionShell(
    "pillars",
    {
      eyebrow: "The 2.0 architecture · LFS-RFC-002",
      title: "Five pillars, one request path",
      lead: "Everything below is implemented and wired into the live path — each pillar carries unit and property tests, and the tools exercise them on real images. <code>src/wiring/</code> puts every 3.0 policy layer on the exact path it governs.",
    },
    `<div class="pillars-grid">${cards}</div>`
  );
}

/* ---------- Capability bento ---------- */

export function renderCapabilities() {
  const tiles = CAPABILITIES.map((c, i) => {
    return reveal(
      `
    <article class="tile span-${c.span}${c.feature ? " feature" : ""}${c.span >= 6 ? " graph" : ""}">
      <div class="tile-head">
        <span class="tile-icon">${icon(c.icon)}</span>
        <h3>${esc(c.title)}</h3>
        <span class="ver">${esc(c.ver)}</span>
      </div>
      <p>${c.desc}</p>
      <div class="stat"><b>${esc(c.statValue)}</b><span>${esc(c.statLabel)}</span></div>
    </article>`,
      i * 70
    );
  }).join("");

  return sectionShell(
    "capabilities",
    {
      center: true,
      eyebrow: "Beyond the pillars · RFC-004 and Phase 14",
      title: `What ${CONFIG.versionShort} actually ships`,
      lead: "The 3.0 additions were consultative; 3.1 wired them onto the live paths; 3.2–3.8 hardened, measured and accelerated them. These are the subsystems — and the honest numbers they produced.",
    },
    `<div class="bento">${tiles}</div>`
  );
}

/* ---------- Architecture explorer ---------- */

export function renderArchitecture() {
  const oob = ARCHITECTURE.outOfBand
    .map((o) => `<div class="arch-oob">${icon(o.icon)}<span><b>${esc(o.label)}</b> — ${esc(o.desc)}</span></div>`)
    .join("");

  const layers = ARCHITECTURE.layers
    .map(
      (l, i) => `
    <button class="arch-layer" role="tab" aria-selected="${i === 2}" data-layer="${i}" id="arch-tab-${i}">
      <span class="idx">${String(i + 1).padStart(2, "0")}</span>
      <span class="name">${esc(l.name)}<small>${esc(l.sub)}</small></span>
      <span class="chev">${icon("chevRight")}</span>
    </button>
    <div class="arch-flow" aria-hidden="true"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14"/><path d="m6 13 6 6 6-6"/></svg></div>`
    )
    .join("");

  return sectionShell(
    "architecture",
    {
      eyebrow: "Top to bottom · one request path",
      title: "The architecture, layer by layer",
      lead: "Every platform difference is confined to the PAL; every 3.0 policy layer sits on the path it governs. Pick a layer to inspect it — the stack below is exactly the order a request travels.",
    },
    `
    <div class="arch-wrap">
      <div class="arch-side" data-reveal="left">
        ${oob}
        <div class="arch-stack" role="tablist" aria-label="Architecture layers">${layers}</div>
      </div>
      <div class="arch-detail" id="archDetail" data-reveal="right" style="--d:120ms" aria-live="polite"></div>
    </div>`
  );
}

export function archDetailHTML(i) {
  const l = ARCHITECTURE.layers[i];
  const chips = l.chips.map((c) => `<span class="arch-chip">${esc(c)}</span>`).join("");
  return `
    <span class="ad-eyebrow">${esc(l.eyebrow)}</span>
    <h3>${esc(l.name)}</h3>
    <p class="ad-sub">${l.desc}</p>
    <div class="arch-chips">${chips}</div>
    <div class="arch-where">${icon("terminal")}<span>in-tree:&nbsp;<b>${l.where}</b></span></div>`;
}

/* ---------- Journey of a write ---------- */

export function renderWritePath() {
  const steps = WRITE_PATH.steps
    .map((s, i) => {
      const routes = s.routes
        ? `<div class="route-split">${s.routes
            .map((r) => `<div class="route-card"><h4>${esc(r.title)}</h4><p>${r.desc}</p></div>`)
            .join("")}</div>`
        : "";
      const cls = s.crash ? "crash" : s.finale ? "finale" : s.routes ? "route routes" : "";
      return reveal(
        `
      <div class="wstep ${cls}">
        <span class="node">${icon(s.icon)}</span>
        <div class="body">
          <h3>${esc(s.title)}<span class="tag${s.crash ? " bad" : ""}">${esc(s.tag)}</span></h3>
          <p>${s.desc}</p>
          ${routes}
        </div>
      </div>`,
        i * 80
      );
    })
    .join("");

  return sectionShell(
    "writepath",
    {
      eyebrow: "The 3.1 wiring · end to end",
      title: "How one write survives",
      lead: "From the VFS surface to the durable device write — with a deterministic power cut in the middle, because that is the step that matters. The GC loop runs this same path as a Bulk-class background circuit.",
    },
    `<div class="wpath"><div class="wpath-rail" aria-hidden="true"></div>${steps}</div>`
  );
}

/* ---------- Performance ---------- */

function barCell(who, w, value, unit, { best = false, on = false, gain = "", fill = "v38" } = {}) {
  return `
  <div class="bar-cell">
    <span class="who${on ? " on" : ""}">${esc(who)}</span>
    <div class="bar-track"><div class="bar-fill ${fill}" data-w="${w.toFixed(1)}"></div></div>
    <span class="bval${best ? " best" : ""}">${esc(value)} <em>${esc(unit)}</em>${gain}</span>
  </div>`;
}

const gainChip = (txt) => `<span class="gain">${esc(txt)}</span>`;

function benchRowHTML(row, mode) {
  let cells = "";
  if (mode === "improve") {
    const max = Math.max(row.v37, row.v38);
    const pct = row.lowerBetter
      ? `−${(((row.v37 - row.v38) / row.v37) * 100).toFixed(0)}%`
      : `+${(((row.v38 - row.v37) / row.v37) * 100).toFixed(0)}%`;
    cells =
      barCell("3.7", (row.v37 / max) * 100, fmt(row.v37), row.unit, { fill: "v37" }) +
      barCell("3.8", (row.v38 / max) * 100, fmt(row.v38), row.unit, { best: true, on: true, gain: gainChip(pct) });
  } else {
    const max = Math.max(row.lion, row.ext4);
    const lionBest = row.lowerBetter ? row.lion < row.ext4 : row.lion > row.ext4;
    cells =
      barCell("LionFS", (row.lion / max) * 100, fmt(row.lion), row.unit, { best: lionBest, on: true }) +
      barCell("ext4", (row.ext4 / max) * 100, fmt(row.ext4), row.unit, { best: !lionBest, fill: "vext" });
  }
  return `
  <div class="bench-row" data-reveal>
    <div class="br-head">
      <span class="wl">${esc(row.wl)}</span>
      <span class="note">${row.note}</span>
    </div>
    ${cells}
  </div>`;
}

export function renderPerformance() {
  const kpis = PERFORMANCE.kpis
    .map(
      (k, i) => reveal(
        `
      <div class="kpi">
        <span class="delta">${icon("check")} ${esc(k.delta)}</span>
        <span class="val"><span data-count="${k.value}">0</span><span class="u">${esc(k.unit)}</span></span>
        <span class="lbl">${esc(k.label)}</span>
      </div>`,
        i * 80
      )
    )
    .join("");

  const panels = PERFORMANCE.tabs
    .map((t, ti) => {
      const rows = t.rows.map((r) => benchRowHTML(r, t.id === "improve" ? "improve" : "versus")).join("");
      const cantdo = t.cantdo
        ? `<div class="cantdo" data-reveal>${t.cantdo
            .map((c) => `<div class="cd"><b>${esc(c.value)}</b><span>${esc(c.label)}</span></div>`)
            .join("")}</div>`
        : "";
      const legend =
        t.id === "improve"
          ? `<div class="bench-legend"><span class="lg"><span class="sw sw-37"></span>LionFS 3.7</span><span class="lg"><span class="sw sw-38"></span>LionFS 3.8</span></div><p class="muted" style="font-size:.78rem;margin:-12px 0 18px">${t.note}</p>`
          : `<div class="bench-legend"><span class="lg"><span class="sw sw-38"></span>LionFS 3.8 (engine)</span><span class="lg"><span class="sw sw-ext"></span>ext4 (kernel)</span></div><p class="muted" style="font-size:.78rem;margin:-12px 0 18px">${t.note}</p>`;
      return `
      <div class="bench-panel${ti === 0 ? " active" : ""}" id="bench-${t.id}" role="tabpanel">
        ${legend}
        ${rows}
        ${cantdo}
      </div>`;
    })
    .join("");

  const tabs = PERFORMANCE.tabs
    .map(
      (t, i) =>
        `<button class="tab" role="tab" aria-selected="${i === 0}" data-bench="${t.id}">${esc(t.label)}</button>`
    )
    .join("");

  const engine = PERFORMANCE.engine;
  const engineRows = engine.rows
    .map((r) => `<span class="rel-tests">${icon("bolt")} ${esc(r.label)} <b>${esc(r.value)}</b></span>`)
    .join("");

  return sectionShell(
    "performance",
    {
      eyebrow: "Measured, not marketed",
      title: "The throughput release, in numbers",
      lead: "Same harness, same host, medians of 3 — every number below is a command you can re-run. 3.8 closed the gaps a same-host benchmark against ext4 exposed: WAL v2, data elision, the node cache.",
    },
    `
    <div class="kpi-row">${kpis}</div>
    <div class="tabs" role="tablist" aria-label="Benchmark views" data-reveal>${tabs}</div>
    ${panels}
    <div class="honesty" style="margin-top:30px" data-reveal>
      <div class="honesty-icon">${icon("bolt")}</div>
      <div>
        <h3>${esc(engine.title)}</h3>
        <p>${esc(engine.desc)}</p>
        <div class="rel-meta" style="margin-top:10px; display:flex; flex-wrap:wrap; gap:14px">${engineRows}</div>
      </div>
    </div>
    <div class="bench-callout" data-reveal>
      ${icon("info")}
      <p>${PERFORMANCE.callout}</p>
    </div>`
  );
}

/* ---------- Comparison matrix ---------- */

function cmpCell(v, isLion) {
  const lower = String(v).toLowerCase();
  let mark = "";
  let cls = "";
  let text = esc(v);
  if (lower === "yes") {
    mark = `<span class="yes">${icon("check")}</span>`;
    cls = "y";
    text = "";
  } else if (lower === "no") {
    mark = `<span class="no">${icon("x")}</span>`;
    cls = "n";
    text = "";
  } else if (lower.startsWith("yes")) {
    mark = `<span class="yes">${icon("check")}</span>`;
    cls = "y";
    text = esc(v.replace(/^yes[—:\s-]*/i, ""));
  } else if (lower.startsWith("no")) {
    mark = `<span class="no">${icon("x")}</span>`;
    cls = "n";
    text = esc(v.replace(/^no[—:\s·-]*/i, ""));
  }
  return `<td${isLion ? ' class="lion"' : ""}><span class="v ${cls}">${mark}${text}</span></td>`;
}

export function renderComparison() {
  const head = `<tr>
    <th class="feat-h">Feature</th>
    ${COMPARISON.systems.map((s, i) => `<th class="${i === 0 ? "lion" : ""}">${esc(s)}</th>`).join("")}
  </tr>`;

  const body = COMPARISON.groups
    .map((g) => {
      const groupRow = `<tr class="group-row" data-group="${esc(g.group)}"><td colspan="${COMPARISON.systems.length + 1}">${esc(g.group)}</td></tr>`;
      const rows = g.features
        .map(
          (f) =>
            `<tr data-group="${esc(g.group)}"><td class="feat">${esc(f.f)}</td>${f.v
              .map((v, i) => cmpCell(v, i === 0))
              .join("")}</tr>`
        )
        .join("");
      return groupRow + rows;
    })
    .join("");

  const chips = ["all", ...COMPARISON.groups.map((g) => g.group)]
    .map((c, i) => `<button class="fchip" data-cmp="${esc(c)}" aria-selected="${i === 0}">${esc(c === "all" ? "All" : c)}</button>`)
    .join("");

  return sectionShell(
    "compare",
    {
      eyebrow: "Feature parity · code-verifiable",
      title: "LionFS vs the field",
      lead: "Qualitative rows — verifiable by reading the code — against ext4, XFS, Btrfs, ZFS, NTFS, ReFS, APFS and RedoxFS. The last row is the one that matters. Scroll sideways for all nine systems.",
    },
    `
    <div class="cmp-filters" data-reveal>${chips}</div>
    <div class="cmp-table-wrap" data-reveal>
      <table class="cmp">
        <thead>${head}</thead>
        <tbody>${body}</tbody>
      </table>
    </div>
    <div class="cmp-note" data-reveal>
      <div class="pull-quote">${COMPARISON.quote}</div>
      <p class="muted" style="font-size:.78rem;margin-top:12px">${COMPARISON.caveats}</p>
    </div>`
  );
}

/* ---------- Releases timeline ---------- */

export function renderReleases() {
  const maxTests = Math.max(...RELEASES.map((r) => r.tests), 1);
  const items = RELEASES.map((r, i) => {
    const cls = r.current ? "current" : r.major ? "major" : "minor";
    const hi = r.hi.map((h) => `<span>${esc(h)}</span>`).join("");
    return reveal(
      `
    <div class="rel ${cls}">
      <span class="dot" aria-hidden="true"></span>
      <div class="rel-card">
        <div class="rel-head">
          <span class="rel-ver">${esc(r.ver)}</span>
          <span class="rel-name">${esc(r.name)}</span>
          ${r.current ? `<span class="rel-badge">current</span>` : ""}
        </div>
        <div class="rel-meta">
          <span class="rel-tests">${icon("checkCircle")} <b>${r.tests ? `${fmt(r.tests)} tests` : "prototype"}</b></span>
          <span class="rel-bar"><i data-w="${((r.tests / maxTests) * 100).toFixed(1)}"></i></span>
        </div>
        <p>${r.desc}</p>
        <div class="hi">${hi}</div>
      </div>
    </div>`,
      i * 70
    );
  }).join("");

  return sectionShell(
    "releases",
    {
      eyebrow: "245 → 462 → … → 840 tests",
      title: "The release line",
      lead: "Every checkbox flips only with the full suite green — the count has only ever moved up. Each release below is a design record in the repo, with the measurements to back it.",
    },
    `<div class="timeline">${items}</div>`
  );
}

/* ---------- Guardian ---------- */

export function renderGuardian() {
  const blips = [
    { top: "18%", left: "63%", d: "0s" },
    { top: "38%", left: "76%", d: "1.1s", c: true },
    { top: "58%", left: "30%", d: "2.2s" },
    { top: "26%", left: "24%", d: "0.6s", c: true },
    { top: "70%", left: "62%", d: "1.7s" },
  ]
    .map(
      (b) =>
        `<span class="blip${b.c ? " c" : ""}" style="top:${b.top};left:${b.left};animation-delay:${b.d}"></span>`
    )
    .join("");

  const cards = GUARDIAN.cards
    .map(
      (c, i) => reveal(
        `
      <div class="g-card">
        <span class="g-icon">${icon(c.icon)}</span>
        <div>
          <h3>${esc(c.title)}</h3>
          <p>${c.desc}</p>
        </div>
      </div>`,
        i * 90
      )
    )
    .join("");

  return sectionShell(
    "guardian",
    {
      eyebrow: "Autonomous operations · RFC-004 §7",
      title: "Guardian — the watchtower",
      lead: GUARDIAN.lead,
    },
    `
    <div class="guardian-wrap">
      <div data-reveal="left">${cards}
        <p class="muted" style="font-size:.8rem;margin-top:6px">${GUARDIAN.foot}</p>
      </div>
      <div data-reveal="right" style="--d:120ms">
        <div class="radar" role="img" aria-label="Guardian advisory radar">
          ${blips}
          <span class="core">${icon("radar")}</span>
        </div>
        <p class="radar-caption">${esc(GUARDIAN.radarCaption)}</p>
      </div>
    </div>`
  );
}

/* ---------- Tools ---------- */

export function renderTools() {
  const cats = TOOLS.categories
    .map((c, i) => `<button class="fchip" data-toolcat="${c}" aria-selected="${i === 0}">${esc(c === "all" ? "All" : c)}</button>`)
    .join("");

  const tools = TOOLS.featured
    .map(
      (t, i) => `
    <article class="tool" data-cat="${esc(t.cat)}" data-name="${esc(t.cmd.toLowerCase())}" style="--d:${(i % 6) * 60}ms" data-reveal>
      <span class="cmd">${esc(t.cmd)}</span>
      <p>${t.desc}</p>
      <span class="tcat">${esc(t.cat)}</span>
    </article>`
    )
    .join("");

  const cloud = TOOLS.allBinaries.map((b) => `<span>${esc(b)}</span>`).join("");

  return sectionShell(
    "tools",
    {
      eyebrow: `${TOOLS.featured.length} featured of ${CONFIG.tools} binaries`,
      title: "The toolbox",
      lead: "Every tool runs against real images through the real mount path — and the changelog names the ones that used to be placeholders, because a tool that prints success without touching the device is a bug, not a feature.",
    },
    `
    <div class="tools-bar" data-reveal>
      <div class="search-box">
        ${icon("search")}
        <input type="search" id="toolSearch" placeholder="Filter tools… (e.g. snapshot, bench, raid)" aria-label="Filter tools" />
      </div>
      <div class="tools-chips">${cats}</div>
    </div>
    <div class="tools-grid" id="toolsGrid">${tools}
      <div class="no-results" id="toolsEmpty" hidden>No tool matches that filter.</div>
    </div>
    <div class="tools-cloud" data-reveal>
      <p class="cl-label">the full toolbox — ${CONFIG.tools} binaries in tools/</p>
      <div class="cloud">${cloud}</div>
    </div>`
  );
}

/* ---------- Quickstart ---------- */

function codeblock(lang, code) {
  const raw = code;
  return `
  <div class="codeblock">
    <div class="cb-head">
      <span class="cb-lang">${esc(lang)}</span>
      <button class="copy-btn" data-copy="${esc(raw)}" aria-label="Copy command">
        ${icon("copy")}<span class="t-copy">copy</span><span class="t-done">copied</span>
      </button>
    </div>
    <pre><code>${highlightBash(code)}</code></pre>
  </div>`;
}

export function renderQuickstart() {
  const steps = QUICKSTART.steps
    .map(
      (s, i) => reveal(
        `
    <div class="qstep">
      <span class="n">${i + 1}</span>
      <div class="qbody">
        <h3>${esc(s.title)}</h3>
        <p>${s.desc}</p>
        ${codeblock(s.lang, s.code)}
      </div>
    </div>`,
        i * 90
      )
    )
    .join("");

  return sectionShell(
    "quickstart",
    {
      eyebrow: "Six steps · Rust 1.75+",
      title: "From clone to crash-proven",
      lead: "Build it, run the suite, format an image, mount it — then let the crash simulator argue with the hardware on your behalf. See <code>BUILD.md</code> for per-platform details.",
    },
    `<div class="qs-steps">${steps}</div>`
  );
}

/* ---------- Docs ---------- */

export function renderDocs() {
  const cards = DOCS.map(
    (d, i) => reveal(
      `
    <a class="doc-card" href="${gh(d.path)}" target="_blank" rel="noopener noreferrer">
      <span class="d-arrow">${icon("arrowUpRight")}</span>
      <span class="d-icon">${icon(d.icon)}</span>
      <h3>${esc(d.title)}</h3>
      <p>${d.desc}</p>
      <span class="d-path">${icon("terminal")}<b>${esc(d.path)}</b></span>
    </a>`,
      i * 70
    )
  ).join("");

  return sectionShell(
    "docs",
    {
      eyebrow: "In-repo, normative",
      title: "Documentation index",
      lead: "The architecture is documented where it lives: RFCs for the normative decisions, specifications for the on-disk format and subsystems, design records for every phase.",
    },
    `<div class="docs-grid">${cards}</div>`
  );
}

/* ---------- Final CTA ---------- */

export function renderCTA() {
  const actions = CTA.actions
    .map((a) => {
      const href = a.external ? gh() : a.href;
      return a.primary
        ? `<a class="btn btn-primary" href="${href}">${esc(a.label)} ${icon("arrowRight")}</a>`
        : `<a class="btn btn-ghost" href="${href}" ${a.external ? 'target="_blank" rel="noopener noreferrer"' : ""}>${icon("github")} ${esc(a.label)}</a>`;
    })
    .join("");

  return sectionShell(
    "cta",
    null,
    `
  <div class="cta-box" data-reveal="zoom">
    <h2>${esc(CTA.title)}</h2>
    <p>${esc(CTA.text)}</p>
    <div class="cta-cmd"><span class="p">$</span> ${esc(CTA.cmd)}</div>
    <div class="cta-actions">${actions}</div>
  </div>`,
    "cta-section"
  );
}

/* ---------- Footer ---------- */

export function renderFooter() {
  const cols = FOOTER.columns
    .map(
      (c) => `
    <div class="footer-col">
      <h4>${esc(c.head)}</h4>
      <ul>
        ${c.links
          .map(
            (l) =>
              `<li><a href="${gh(l.path)}" target="_blank" rel="noopener noreferrer">${icon("arrowRight")}${esc(l.label)}</a></li>`
          )
          .join("")}
      </ul>
    </div>`
    )
    .join("");

  return `
  <div class="container">
    <div class="footer-grid">
      <div class="footer-brand">
        <a class="brand" href="#overview" aria-label="LionFS home">
          <span class="brand-mark">${LION_LOGO}</span>
          <span class="brand-name">Lion<em>FS</em></span>
        </a>
        <p>${esc(FOOTER.tagline)}</p>
      </div>
      ${cols}
    </div>
    <div class="footer-bottom">
      <p>${esc(FOOTER.status)}</p>
      <p class="right">${esc(FOOTER.credit)} · index.html + css/ + js/ — GitHub Pages ready</p>
    </div>
  </div>`;
}

/* ---------- Page assembly ---------- */

export function buildMainHTML() {
  return [
    renderHero(),
    renderTicker(),
    renderHonesty(),
    renderPillars(),
    renderCapabilities(),
    renderArchitecture(),
    renderWritePath(),
    renderPerformance(),
    renderComparison(),
    renderReleases(),
    renderGuardian(),
    renderTools(),
    renderQuickstart(),
    renderDocs(),
    renderCTA(),
  ].join("");
}

