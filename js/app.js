/* ==========================================================================
   LionFS — js/app.js
   Mounts the rendered DOM, then wires every interaction:
   theme, scrollspy, reveals, counters, animated bars, terminal typing,
   tabs, filters, copy buttons, mobile drawer, back-to-top.
   ========================================================================== */


const $  = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));
const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

/* ---------- 1. Mount the DOM ---------- */
$("#navRoot").innerHTML = renderNav();
document.body.insertAdjacentHTML("beforeend", renderDrawer());
$("#main").innerHTML = buildMainHTML();
$("#siteFooter").innerHTML = renderFooter();

/* Remind the maintainer to point links at the real fork. */
if (CONFIG.repoUrl.includes("your-username")) {
  console.warn(
    "%c[LionFS site] reminder: set `repoUrl` at the top of js/data.js to your real GitHub repository so all GitHub links work.",
    "color:#f5a524;font-weight:bold"
  );
}

/* ---------- 2. Theme (toggle only, init moved to index.html) ---------- */
const themeToggle = $("#themeToggle");
const applyTheme = (t) => {
  document.documentElement.setAttribute("data-theme", t);
  try { localStorage.setItem("lionfs-theme", t); } catch { /* private mode */ }
};
themeToggle.addEventListener("click", () => {
  const cur = document.documentElement.getAttribute("data-theme");
  applyTheme(cur === "dark" ? "light" : "dark");
});


/* ---------- 3. Header state + scroll progress + back-to-top ---------- */
const header = $("#siteHeader");
const progress = $("#scrollProgressBar");
const toTop = $("#backToTop");
let ticking = false;

const onScroll = () => {
  const y = window.scrollY;
  header.classList.toggle("scrolled", y > 8);
  toTop.classList.toggle("show", y > 640);
  const max = document.documentElement.scrollHeight - window.innerHeight;
  progress.style.width = `${max > 0 ? (y / max) * 100 : 0}%`;
  ticking = false;
};
window.addEventListener(
  "scroll",
  () => {
    if (!ticking) { ticking = true; requestAnimationFrame(onScroll); }
  },
  { passive: true }
);
onScroll();

toTop.addEventListener("click", () => window.scrollTo({ top: 0, behavior: reduced ? "auto" : "smooth" }));

/* ---------- 4. Mobile drawer ---------- */
const drawer = $("#navDrawer");
const backdrop = $("#drawerBackdrop");
const hamburger = $("#hamburger");
const setDrawer = (open) => {
  drawer.classList.toggle("open", open);
  backdrop.classList.toggle("open", open);
  hamburger.setAttribute("aria-expanded", String(open));
  document.body.style.overflow = open ? "hidden" : "";
};
hamburger.addEventListener("click", () => setDrawer(!drawer.classList.contains("open")));
$("#drawerClose").addEventListener("click", () => setDrawer(false));
backdrop.addEventListener("click", () => setDrawer(false));
$$(".nav-drawer a").forEach((a) => a.addEventListener("click", () => setDrawer(false)));
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") setDrawer(false);
});

/* ---------- 5. Reveal-on-scroll ---------- */
const revealEls = $$("[data-reveal]");
if (reduced) {
  revealEls.forEach((el) => el.classList.add("revealed"));
} else {
  const io = new IntersectionObserver(
    (entries) => {
      entries.forEach((en) => {
        if (en.isIntersecting) {
          en.target.classList.add("revealed");
          io.unobserve(en.target);
        }
      });
    },
    { rootMargin: "0px 0px -8% 0px", threshold: 0.08 }
  );
  revealEls.forEach((el) => io.observe(el));
}

/* ---------- 6. Animated counters ---------- */
const animateCount = (el) => {
  const target = parseFloat(el.dataset.count);
  if (Number.isNaN(target)) return;
  if (reduced) { el.textContent = String(target); return; }
  const dur = 1300;
  const t0 = performance.now();
  const decimals = String(target).includes(".") ? 1 : 0;
  const tick = (t) => {
    const p = Math.min(1, (t - t0) / dur);
    const eased = 1 - Math.pow(1 - p, 3);
    el.textContent = (target * eased).toFixed(decimals);
    if (p < 1) requestAnimationFrame(tick);
    else el.textContent = String(target);
  };
  requestAnimationFrame(tick);
};
const counterIO = new IntersectionObserver(
  (entries) => {
    entries.forEach((en) => {
      if (en.isIntersecting) { animateCount(en.target); counterIO.unobserve(en.target); }
    });
  },
  { threshold: 0.6 }
);
$$("[data-count]").forEach((el) => counterIO.observe(el));

/* ---------- 7. Animated bars (benchmarks + release test counts) ---------- */
const barIO = new IntersectionObserver(
  (entries) => {
    entries.forEach((en) => {
      if (!en.isIntersecting) return;
      const el = en.target;
      const w = el.dataset.w;
      if (w != null) {
        if (reduced) el.style.transition = "none";
        el.style.width = `${w}%`;
      }
      barIO.unobserve(el);
    });
  },
  { threshold: 0.4 }
);
$$(".bar-fill[data-w]").forEach((el) => barIO.observe(el));
$$(".rel-bar i[data-w]").forEach((el) => barIO.observe(el));

/* ---------- 8. Hero terminal typing ---------- */
const termBody = $("#termBody");
const termLines = HERO.terminal.lines;
let typing = false;

function renderTerminalFull() {
  termBody.innerHTML = termLines
    .map((l) => {
      if (l.type === "cmd") {
        return `<span class="tline"><span class="p"><span class="u">lion@storage</span>:~$</span> ${esc(l.text)}</span>`;
      }
      return `<span class="tline ${l.hl ? "hl" : "o"}">${esc(l.text)}</span>`;
    })
    .join("");
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function typeTerminal() {
  if (typing || reduced) { renderTerminalFull(); return; }
  typing = true;
  termBody.innerHTML = "";
  const cursor = document.createElement("span");
  cursor.className = "tcursor";
  for (const l of termLines) {
    const line = document.createElement("span");
    line.className = "tline";
    if (l.type === "cmd") {
      line.innerHTML = `<span class="p"><span class="u">lion@storage</span>:~$</span> `;
      termBody.appendChild(line);
      line.appendChild(cursor);
      for (const ch of l.text) {
        cursor.insertAdjacentText("beforebegin", ch);
        await sleep(26 + Math.random() * 30);
      }
      await sleep(260);
      cursor.remove();
    } else {
      line.className = `tline ${l.hl ? "hl" : "o"}`;
      line.textContent = l.text;
      termBody.appendChild(line);
      await sleep(210);
    }
  }
  termBody.appendChild(cursor);
  typing = false;
}

const termIO = new IntersectionObserver(
  (entries) => {
    entries.forEach((en) => {
      if (en.isIntersecting) { typeTerminal(); termIO.disconnect(); }
    });
  },
  { threshold: 0.35 }
);
if (termBody) termIO.observe(termBody);

const replayBtn = $("#termReplay");
if (replayBtn) {
  replayBtn.addEventListener("click", () => {
    if (!typing) { termBody.innerHTML = ""; typeTerminal(); }
  });
}

/* ---------- 9. Benchmark tabs ---------- */
$$(".tab[data-bench]").forEach((tab) => {
  tab.addEventListener("click", () => {
    $$(".tab[data-bench]").forEach((t) => t.setAttribute("aria-selected", "false"));
    tab.setAttribute("aria-selected", "true");
    $$(".bench-panel").forEach((p) => p.classList.remove("active"));
    const panel = $(`#bench-${tab.dataset.bench}`);
    if (panel) {
      panel.classList.add("active");
      // re-trigger bar fills for the newly shown panel
      $$(".bar-fill[data-w]", panel).forEach((el) => {
        el.style.width = `${el.dataset.w}%`;
      });
    }
  });
});

/* ---------- 10. Comparison filter chips ---------- */
$$(".fchip[data-cmp]").forEach((chip) => {
  chip.addEventListener("click", () => {
    $$(".fchip[data-cmp]").forEach((c) => c.setAttribute("aria-selected", "false"));
    chip.setAttribute("aria-selected", "true");
    const g = chip.dataset.cmp;
    $$("#compare tbody tr").forEach((tr) => {
      const isGroup = tr.classList.contains("group-row");
      if (g === "all") { tr.hidden = false; return; }
      tr.hidden = tr.dataset.group !== g;
      if (isGroup && tr.dataset.group === g) tr.hidden = false;
    });
  });
});

/* ---------- 11. Tools: search + category filter ---------- */
const toolSearch = $("#toolSearch");
let toolCat = "all";

function applyToolFilter() {
  const q = (toolSearch?.value || "").trim().toLowerCase();
  let visible = 0;
  $$("#toolsGrid .tool").forEach((card) => {
    const matchCat = toolCat === "all" || card.dataset.cat === toolCat;
    const matchQ =
      !q ||
      card.dataset.name.includes(q) ||
      card.textContent.toLowerCase().includes(q);
    const show = matchCat && matchQ;
    card.style.display = show ? "" : "none";
    if (show) visible++;
  });
  const empty = $("#toolsEmpty");
  if (empty) empty.hidden = visible !== 0;
}

if (toolSearch) {
  toolSearch.addEventListener("input", applyToolFilter);
  // "/" focuses the search box, like every good docs site
  window.addEventListener("keydown", (e) => {
    if (e.key === "/" && document.activeElement === document.body) {
      e.preventDefault();
      toolSearch.focus();
    }
  });
}
$$(".fchip[data-toolcat]").forEach((chip) => {
  chip.addEventListener("click", () => {
    $$(".fchip[data-toolcat]").forEach((c) => c.setAttribute("aria-selected", "false"));
    chip.setAttribute("aria-selected", "true");
    toolCat = chip.dataset.toolcat;
    applyToolFilter();
  });
});

/* ---------- 12. Copy buttons (event delegation) ---------- */
document.addEventListener("click", async (e) => {
  const btn = e.target.closest(".copy-btn");
  if (!btn) return;
  const text = btn.dataset.copy || "";
  const ok = await copyText(text);
  if (ok) {
    btn.classList.add("copied");
    setTimeout(() => btn.classList.remove("copied"), 1800);
  }
});

/* ---------- 13. Architecture explorer ---------- */
const archDetail = $("#archDetail");
const archTabs = $$("[data-layer]");
function selectLayer(i) {
  archTabs.forEach((t) => t.setAttribute("aria-selected", "false"));
  const tab = $(`[data-layer="${i}"]`);
  if (tab) tab.setAttribute("aria-selected", "true");
  if (archDetail) archDetail.innerHTML = archDetailHTML(i);
}
archTabs.forEach((t) =>
  t.addEventListener("click", () => selectLayer(parseInt(t.dataset.layer, 10)))
);
selectLayer(2); // start on "Wiring" — the layer that makes 3.x interesting

/* ---------- 14. Scrollspy + smooth anchors ---------- */
const navLinks = $$("[data-nav]");
const sections = NAV.map((n) => $(`#${n.id}`)).filter(Boolean);

const spyIO = new IntersectionObserver(
  (entries) => {
    entries.forEach((en) => {
      if (!en.isIntersecting) return;
      const id = en.target.id;
      navLinks.forEach((a) => a.classList.toggle("active", a.dataset.nav === id));
    });
  },
  { rootMargin: "-42% 0px -52% 0px" }
);
sections.forEach((s) => spyIO.observe(s));

// Keyboard focus: arrows move between architecture layers
const stack = $(".arch-stack");
if (stack) {
  stack.addEventListener("keydown", (e) => {
    const cur = parseInt($("[data-layer][aria-selected='true']")?.dataset.layer || "2", 10);
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const next = Math.min(Math.max(cur + (e.key === "ArrowDown" ? 1 : -1), 0), ARCHITECTURE.layers.length - 1);
      selectLayer(next);
      $(`[data-layer="${next}"]`)?.focus();
    }
  });
}
