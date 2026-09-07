/* ==========================================================================
   LionFS — js/utils.js
   Small helpers: HTML escaping, clipboard, formatting, SVG icon set.
   ========================================================================== */

/** Escape a string for safe innerHTML interpolation. */
export const esc = (s) =>
  String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));

/** Copy text to clipboard with a legacy fallback (file:// contexts). */
export async function copyText(text) {
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
export const fmt = (n) =>
  Number(n).toLocaleString("en-US", { maximumFractionDigits: 1 });

/** Simple bash-syntax highlighter → HTML string (token spans).
    Understands: comments, $ prompts, flags, strings, numbers, paths, pipes. */
export function highlightBash(code) {
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
   Keep icons tiny and consistent: 24×24 viewBox, 2px stroke, round caps.
   ------------------------------------------------------------------------ */
const I = {
  bolt: '<path d="M13 2 3 14h7l-1 8 10-12h-7l1-8z"/>',
  scale: '<path d="M12 3v18"/><path d="M5 7l7-4 7 4"/><path d="M3 11l2-4 2 4a2 2 0 1 1-4 0z"/><path d="M17 11l2-4 2 4a2 2 0 1 1-4 0z"/><path d="M8 21h8"/>',
  shield: '<path d="M12 22s8-3.5 8-10V5l-8-3-8 3v7c0 6.5 8 10 8 10z"/><path d="m9 12 2 2 4-4"/>',
  layers: '<path d="m12 2 9 5-9 5-9-5 9-5z"/><path d="m3 12 9 5 9-5"/><path d="m3 17 9 5 9-5"/>',
  pipeline: '<path d="M4 6h8a4 4 0 0 1 4 4v4a4 4 0 0 0 4 4h0"/><path d="M4 18h4"/><circle cx="4" cy="6" r="1.6"/><circle cx="20" cy="18" r="1.6"/>',
  journal: '<path d="M6 2h12v20l-3-2-3 2-3-2-3 2V2z"/><path d="M9 7h6"/><path d="M9 11h6"/><path d="M9 15h3"/>',
  journalSm: '<path d="M6 2h12v20l-3-2-3 2-3-2-3 2V2z"/><path d="M9 7h6"/><path d="M9 11h6"/>',
  elide: '<path d="M13 2 3 14h7l-1 8 10-12h-7l1-8z"/>',
  cache: '<ellipse cx="12" cy="5" rx="8" ry="3"/><path d="M4 5v14c0 1.7 3.6 3 8 3s8-1.3 8-3V5"/><path d="M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3"/>',
  qos: '<path d="M12 2a10 10 0 1 0 10 10"/><path d="M12 12 21 3"/><circle cx="12" cy="12" r="3"/>',
  globe: '<circle cx="12" cy="12" r="10"/><path d="M2 12h20"/><path d="M12 2a15 15 0 0 1 0 20 15 15 0 0 1 0-20z"/>',
  gc: '<path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5"/><path d="M12 7v5l3 3"/>',
  radar: '<circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="5"/><circle cx="12" cy="12" r="1.4"/><path d="M12 12 18 6"/>',
  chart: '<path d="M3 3v18h18"/><path d="M7 15v-4"/><path d="M12 15V7"/><path d="M17 15v-7"/>',
  migrate: '<path d="M12 3 6 9h12l-6-6z"/><path d="M12 21l6-6H6l6 6z"/>',
  container: '<rect x="3" y="8" width="18" height="12" rx="2"/><path d="M7 8V4h10v4"/><path d="M3 13h18"/>',
  key: '<circle cx="8" cy="15" r="4"/><path d="m11 12 9-9"/><path d="M17 6l3 3"/><path d="M15 8l2 2"/>',
  clock: '<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 3"/>',
  balance: '<path d="M12 3v18"/><path d="M6 8l-3 6a3 3 0 0 0 6 0L6 8z"/><path d="M18 8l-3 6a3 3 0 0 0 6 0l-3-6z"/><path d="M7 4h10"/>',
  inbox: '<path d="M22 12h-6l-2 3h-4l-2-3H2"/><path d="M5 5h14l3 7v7H2v-7l3-7z"/>',
  gate: '<path d="M12 22s8-3.5 8-10V5l-8-3-8 3v7c0 6.5 8 10 8 10z"/>',
  route: '<circle cx="6" cy="19" r="2.4"/><circle cx="18" cy="5" r="2.4"/><path d="M8.4 19H15a4 4 0 0 0 0-8H9a4 4 0 0 1 0-8h6.6"/>',
  commit: '<circle cx="12" cy="12" r="3.4"/><path d="M12 2v6.6"/><path d="M12 15.4V22"/><path d="M2 12h6.6"/><path d="M15.4 12H22"/>',
  replay: '<path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5"/><path d="M12 7v5l4 2"/>',
  zap: '<path d="M13 2 3 14h7l-1 8 10-12h-7l1-8z"/>',
  check: '<path d="m4 12 5 5L20 6"/>',
  checkCircle: '<circle cx="12" cy="12" r="9"/><path d="m8 12 3 3 5-6"/>',
  x: '<path d="M6 6l12 12"/><path d="M18 6 6 18"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 8h.01"/><path d="M12 12v5"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="m20 20-4-4"/>',
  copy: '<rect x="9" y="9" width="12" height="12" rx="2"/><path d="M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1"/>',
  arrowRight: '<path d="M5 12h14"/><path d="m13 6 6 6-6 6"/>',
  arrowUpRight: '<path d="M7 17 17 7"/><path d="M8 7h9v9"/>',
  sun: '<circle cx="12" cy="12" r="4.5"/><path d="M12 2v2.5"/><path d="M12 19.5V22"/><path d="m4.9 4.9 1.8 1.8"/><path d="m17.3 17.3 1.8 1.8"/><path d="M2 12h2.5"/><path d="M19.5 12H22"/><path d="m4.9 19.1 1.8-1.8"/><path d="m17.3 6.7 1.8-1.8"/>',
  moon: '<path d="M21 13A9 9 0 1 1 11 3a7 7 0 0 0 10 10z"/>',
  menu: '<path d="M4 7h16"/><path d="M4 12h16"/><path d="M4 17h16"/>',
  close: '<path d="M6 6l12 12"/><path d="M18 6 6 18"/>',
  github: '<path d="M9 19c-5 1.5-5-2.5-7-3m14 6v-3.9a3.4 3.4 0 0 0-1-2.6c3-.3 6.2-1.5 6.2-6.7a5.2 5.2 0 0 0-1.4-3.6 4.9 4.9 0 0 0-.1-3.7s-1.2-.4-3.9 1.5a13.4 13.4 0 0 0-7 0C6.1 1.7 4.9 2.1 4.9 2.1a4.9 4.9 0 0 0-.1 3.7A5.2 5.2 0 0 0 3.4 9.4c0 5.2 3.2 6.4 6.2 6.7a3.4 3.4 0 0 0-1 2.6V22"/><path d="M12 8v8"/><path d="M8.5 11.5h7"/>',
  external: '<path d="M7 17 17 7"/><path d="M8 7h9v9"/>',
  book: '<path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20V2H6.5A2.5 2.5 0 0 0 4 4.5v15z"/><path d="M4 19.5A2.5 2.5 0 0 0 6.5 22H20v-5"/>',
  scroll: '<path d="M6 2h12v20l-3-2-3 2-3-2-3 2V2z"/><path d="M9 7h6"/><path d="M9 11h6"/>',
  grid: '<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/>',
  map: '<path d="m9 3 6 2 6-2v16l-6 2-6-2-6 2V5l6-2z"/><path d="M9 3v16"/><path d="M15 5v16"/>',
  hammer: '<path d="m15 12-8.5 8.5a2.1 2.1 0 0 1-3-3L12 9"/><path d="m17.6 14.6 3.4-3.4-8-8-3.4 3.4 8 8z"/>',
  virus: '<circle cx="12" cy="12" r="4"/><path d="M12 2v2.5"/><path d="M12 19.5V22"/><path d="M2 12h2.5"/><path d="M19.5 12H22"/><path d="m5 5 1.8 1.8"/><path d="m17.2 17.2 1.8 1.8"/><path d="m19 5-1.8 1.8"/><path d="m6.8 17.2-1.8 1.8"/>',
  pulse: '<path d="M3 12h4l2-7 4 14 2-7h6"/>',
  cpu: '<rect x="6" y="6" width="12" height="12" rx="2"/><rect x="10" y="10" width="4" height="4"/><path d="M12 2v2.5"/><path d="M12 19.5V22"/><path d="M2 12h2.5"/><path d="M19.5 12H22"/><path d="m4.9 4.9 1.8 1.8"/><path d="m17.3 17.3 1.8 1.8"/><path d="m4.9 19.1 1.8-1.8"/><path d="m17.3 6.7 1.8-1.8"/>',
  chevRight: '<path d="m9 6 6 6-6 6"/>',
  chevDown: '<path d="m6 9 6 6 6-6"/>',
  refresh: '<path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5"/>',
  terminal: '<path d="m5 7 5 5-5 5"/><path d="M13 17h6"/>',
};

/** Render an icon by name: icon("bolt") → svg string. */
export const icon = (name, cls = "") =>
  `<svg class="ic ${cls}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${I[name] || I.check}</svg>`;

/** The LionFS mark — a geometric lion (12-spike mane star + face). */
export const LION_LOGO = `
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
on by namte