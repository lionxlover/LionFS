/* ==========================================================================
   LionFS — js/utils.js
   Small helpers: HTML escaping, clipboard, formatting, SVG icon set.
   ========================================================================== */

/** Escape a string for safe innerHTML interpolation. */
const esc = (s) =>
  String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));

/** Copy text to clipboard with a legacy fallback (file:// contexts). */
async function copyText(text) {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch { /* fall through to legacy path */ }
  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.cssText = "position:fixed;left:-9999px;opacity:0";
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    ta.remove();
    return ok;
  } catch {
    return false;
  }
}

/** Format a number with thousands separators. */
const fmt = (n) =>
  Number(n).toLocaleString("en-US", { maximumFractionDigits: 1 });

/** Simple bash-syntax highlighter → HTML string (token spans).
    Understands: comments, $ prompts, flags, strings, numbers, paths, pipes. */
function highlightBash(code) {
  const lines = esc(code).split("\n");
  const KNOWN = new Set([
    "cargo", "sudo", "rustc", "target/release/mkfs_lfs", "target/release/mount_lfs",
    "mkfs_lfs", "mount_lfs", "lfs_versus", "lfs_simulate", "lfs_snapshot", "lfs_guardian",
    "./target/release/lfs_versus", "./target/release/lfs_simulate", "cargo", "true",
  ]);
  return lines
    .map((raw) => {
      if (!raw.trim()) return "<span class='ln'>&nbsp;</span>";
      // comments
      if (raw.trim().startsWith("#")) return `<span class="ln tk-com">${raw}</span>`;
      let out = raw;
      // prompt chars (none here, we render whole lines) — flags
      out = out.replace(/(^|\s)(--?[A-Za-z0-9_-]+)/g, "$1<span class='tk-flag'>$2</span>");
      // strings
      out = out.replace(/(&quot;[^&]*&quot;|&#39;[^&]*&#39;)/g, "<span class='tk-str'>$1</span>");
      // paths
      out = out.replace(/(\s)(\/[A-Za-z0-9_.\-\/]*)/g, "$1<span class='tk-path'>$2</span>");
      // numbers
      out = out.replace(/(\s)(\d+)(\s|$)/g, "$1<span class='tk-num'>$2</span>$3");
      // known binaries (first word-ish tokens)
      out = out.replace(/^(\s*)((?:\.\/)?[A-Za-z0-9_\-\/]*)(?=\s|$)/, (m, pre, tok) => {
        if (KNOWN.has(tok) || KNOWN.has(tok.replace(/^\.\//, ""))) {
          return `${pre}<span class="tk-cmd">${tok}</span>`;
        }
        return m;
      });
      return `<span class="ln">${out}</span>`;
    })
    .join("");
}

/* ---- Inline SVG icon set (stroke-based, currentColor) --------------------
   Font Awesome Solid class names, one per icon key used across the
   site. `github` is the one brand icon (fa-brands, not fa-solid).
   ------------------------------------------------------------------------ */
const I = {
  bolt: "bolt",
  scale: "scale-balanced",
  shield: "shield-halved",
  layers: "layer-group",
  pipeline: "diagram-project",
  journal: "book",
  journalSm: "book",
  elide: "bolt",
  cache: "database",
  qos: "gauge-high",
  globe: "globe",
  gc: "recycle",
  radar: "crosshairs",
  chart: "chart-column",
  migrate: "arrows-up-down",
  container: "box",
  key: "key",
  clock: "clock",
  balance: "scale-balanced",
  inbox: "inbox",
  gate: "shield",
  route: "route",
  commit: "code-commit",
  replay: "clock-rotate-left",
  zap: "bolt",
  check: "check",
  checkCircle: "circle-check",
  x: "xmark",
  info: "circle-info",
  search: "magnifying-glass",
  copy: "copy",
  arrowRight: "arrow-right",
  arrowUpRight: "up-right-from-square",
  sun: "sun",
  moon: "moon",
  menu: "bars",
  close: "xmark",
  github: "github",
  external: "up-right-from-square",
  book: "book",
  scroll: "scroll",
  grid: "table-cells-large",
  map: "map",
  hammer: "hammer",
  virus: "virus",
  pulse: "wave-square",
  cpu: "microchip",
  chevRight: "chevron-right",
  chevDown: "chevron-down",
  refresh: "arrows-rotate",
  terminal: "terminal",
  trophy: "trophy",
};

/** Render an icon by name: icon("bolt") → <i> string. Every icon is
    Font Awesome Solid except `github`, which is a brand mark. */
const icon = (name, cls = "") => {
  const glyph = I[name] || I.check;
  const style = name === "github" ? "fa-brands" : "fa-solid";
  return `<i class="ic ${style} fa-${glyph} ${cls}" aria-hidden="true"></i>`;
};

/** The LionFS mark — a geometric lion (12-spike mane star + face). */
const LION_LOGO = `
<svg viewBox="0 0 64 64" fill="none" aria-hidden="true">
  <defs>
    <linearGradient id="lion-g" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#ffc24d"/>
      <stop offset="1" stop-color="#f5880e"/>
    </linearGradient>
  </defs>
  <polygon fill="url(#lion-g)" points="32,6 37.2,14.7 46,9.7 46.1,19.9 56.2,20 51.3,28.8 60,34 51.3,39.2 56.2,48 46.1,48.1 46,58.2 37.2,53.3 32,62 26.8,53.3 18,58.2 17.9,48.1 7.8,48 12.7,39.2 4,34 12.7,28.8 7.8,20 17.9,19.9 18,9.7 26.8,14.7"/>
  <circle cx="32" cy="34" r="13.5" fill="#0a0c10"/>
  <rect x="23.4" y="29.2" width="6.4" height="2.7" rx="1.35" fill="url(#lion-g)" transform="rotate(-12 26.6 30.55)"/>
  <rect x="34.2" y="29.2" width="6.4" height="2.7" rx="1.35" fill="url(#lion-g)" transform="rotate(12 37.4 30.55)"/>
  <path d="M29.2 36.6h5.6L32 39.9z" fill="url(#lion-g)"/>
  <path d="M32 39.9v2.4m0 0c-1.1 1.3-2.9 1.4-4 .5m4-.5c1.1 1.3 2.9 1.4 4 .5" stroke="url(#lion-g)" stroke-width="1.7" stroke-linecap="round"/>
</svg>`;
