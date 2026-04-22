//! Inline stylesheet for the dashboard.

pub(super) const CSS: &str = r#"
:root {
  color-scheme: light dark;
  --bg: #f7f8fa; --surface: #fff; --surface-2: #f2f4f7;
  --border: #e1e4e8; --border-strong: #cfd6de;
  --text: #1f2328; --text-2: #59636e; --text-3: #848f99;
  --accent: #0969da;
  --danger: #cf222e; --danger-bg: #ffeef0;
  --ok: #1a7f37; --ok-bg: #dafbe1;
  --warn: #9a6700; --warn-bg: #fff8c5;
  --def: #0969da; --def-bg: #ddf4ff;
  --ring: color-mix(in srgb, var(--accent) 30%, transparent);
  /* Typography tokens — `--text-*` sizes name roles, not values,
     so a scale change doesn't require a repo-wide find/replace. */
  --text-display: 1.75rem;
  --text-heading: 1.0625rem;
  --text-body: 0.9375rem;
  --text-small: 0.8125rem;
  --text-micro: 0.72rem;
  --font-sans: -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
  --font-mono: ui-monospace, "SF Mono", "Cascadia Mono", Menlo, monospace;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0d1117; --surface: #161b22; --surface-2: #1c2128;
    --border: #30363d; --border-strong: #484f58;
    --text: #e6edf3; --text-2: #9198a1; --text-3: #6e7681;
    --accent: #58a6ff;
    --danger: #f85149; --danger-bg: #3b1316;
    --ok: #3fb950; --ok-bg: #033a16;
    --warn: #d29922; --warn-bg: #3b2e01;
    --def: #58a6ff; --def-bg: #0b2a4a;
  }
}
*, *::before, *::after { box-sizing: border-box; }
html { font-size: 16px; }
body {
  font-family: var(--font-sans);
  font-size: var(--text-body); line-height: 1.55;
  color: var(--text); background: var(--bg);
  max-width: 60rem; margin: 0 auto;
  padding: 2.5rem 1.25rem 3rem;
  -webkit-font-smoothing: antialiased;
  -moz-osx-font-smoothing: grayscale;
  font-kerning: normal;
  font-feature-settings: "kern", "liga", "calt";
}
code {
  font-family: var(--font-mono);
  font-size: 0.875em; background: var(--surface-2);
  padding: 0.1em 0.4em; border-radius: 3px;
  word-break: break-word;
}
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
:focus-visible { outline: 2px solid var(--ring); outline-offset: 2px; border-radius: 3px; }

/* HEAD — H1 gets typographic weight (the one H1 on the page).
   Name stays in --accent for a visual anchor; everything else is
   metadata in the smaller, dimmer secondary line. */
.page-head {
  display: flex; align-items: center; gap: 0.875rem;
  padding-bottom: 1.5rem; border-bottom: 1px solid var(--border);
  margin-bottom: 2rem;
}
.dot { width: 10px; height: 10px; border-radius: 50%; flex-shrink: 0; }
.dot.ok   { background: var(--ok);   box-shadow: 0 0 0 4px color-mix(in srgb, var(--ok) 22%, transparent); }
.dot.warn { background: var(--warn); box-shadow: 0 0 0 4px color-mix(in srgb, var(--warn) 22%, transparent); }
h1 {
  font-size: var(--text-display); font-weight: 700; line-height: 1.1;
  margin: 0; letter-spacing: -0.02em; color: var(--accent);
  font-feature-settings: "kern", "liga", "ss01";
}
.page-meta { margin: 0.125rem 0 0; color: var(--text-2); font-size: var(--text-small); font-variant-numeric: tabular-nums; }
.page-meta strong { color: var(--text); font-weight: 600; }
.page-meta .sep { margin: 0 0.4em; color: var(--text-3); }

/* SECTIONS — H2 labels identify groups; kept small-caps-style for a
   "field label" feel without shouting. Generous margin below so the
   label and the panel feel like one unit, not two. */
section { margin-bottom: 2.25rem; }
section:last-of-type { margin-bottom: 0; }
h2 {
  font-size: var(--text-micro); font-weight: 600; letter-spacing: 0.1em;
  text-transform: uppercase; color: var(--text-2); margin: 0 0 0.625rem;
}
.section-lede { margin: -0.25rem 0 0.625rem; color: var(--text-2); font-size: var(--text-small); }
.panel {
  background: var(--surface); border: 1px solid var(--border);
  border-radius: 8px; overflow: hidden;
}

/* STATS — big mono value, tiny label above. Matches the read order
   a user expects ("what am I looking at" → "what's the number"). */
.stats { display: grid; grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 0.625rem; }
.stat { background: var(--surface); border: 1px solid var(--border); border-radius: 8px; padding: 0.875rem 1rem 0.9rem; }
.stat-label { font-size: var(--text-micro); font-weight: 600; letter-spacing: 0.05em; text-transform: uppercase; color: var(--text-2); }
.stat-value {
  margin-top: 0.375rem;
  font-family: var(--font-mono);
  font-size: 1.3125rem; font-weight: 600;
  font-variant-numeric: tabular-nums; line-height: 1.15;
  color: var(--text); overflow-wrap: anywhere;
  letter-spacing: -0.01em;
}
.stat-sub { margin-top: 0.125rem; color: var(--text-3); font-size: var(--text-small); font-variant-numeric: tabular-nums; }

/* DL — field labels and values, flat (no tinted background / border
   on the dt). Relies on color + weight for hierarchy, which reads as
   "labelled field" instead of "data table cell". */
dl { display: grid; grid-template-columns: minmax(11rem, max-content) 1fr; margin: 0; }
dt, dd { padding: 0.5rem 1rem; margin: 0; border-top: 1px solid var(--border); }
dt {
  color: var(--text-2); font-size: var(--text-small); font-weight: 500;
  white-space: nowrap;
}
dd { color: var(--text); overflow-wrap: anywhere; font-variant-numeric: tabular-nums; }
dd code { overflow-wrap: anywhere; word-break: break-all; }
dt:first-of-type, dt:first-of-type + dd { border-top: 0; }
dd.muted, .muted { color: var(--text-3); }
dd.muted { font-style: italic; }
@media (max-width: 560px) {
  dl { grid-template-columns: 1fr; }
  dt { padding-bottom: 0.15rem; }
  dt:first-of-type { border-top: 1px solid var(--border); }
  dd { padding-top: 0; border-top: 0; }
  dd + dt { border-top: 1px solid var(--border); }
}

/* TABLE — session listing. Header row in small caps, tight body, mono
   for session names so eye-scanning down the column is stable. */
table { width: 100%; border-collapse: collapse; }
thead th {
  padding: 0.45rem 1rem; text-align: left;
  font-size: var(--text-micro); font-weight: 600; letter-spacing: 0.06em;
  text-transform: uppercase; color: var(--text-2);
  background: var(--surface-2); border-bottom: 1px solid var(--border);
}
thead th.col-actions { text-align: right; }
tbody td { padding: 0.5rem 1rem; border-bottom: 1px solid var(--border); vertical-align: middle; }
tbody tr:last-child td { border-bottom: 0; }
td.col-name {
  font-family: var(--font-mono); font-weight: 500; color: var(--text);
  overflow-wrap: anywhere; word-break: break-all;
  font-feature-settings: "ss02";
}
td.col-name.muted { color: var(--text-3); font-weight: 400; }
td.col-actions { text-align: right; white-space: nowrap; width: 0; }
td.col-empty { padding: 1.5rem 1rem; text-align: center; color: var(--text-3); font-style: italic; }

/* BADGES — pill tags, slightly tighter so "running / allowlisted / default"
   can all fit on one row without wrapping in a narrow viewport. */
.badge {
  display: inline-block; padding: 2px 8px; border-radius: 10px;
  font-size: var(--text-micro); font-weight: 600; margin-right: 0.25rem;
  letter-spacing: 0.02em;
}
.badge:last-child { margin-right: 0; }
.badge-ok   { background: var(--ok-bg);     color: var(--ok); }
.badge-miss { background: var(--danger-bg); color: var(--danger); }
.badge-def  { background: var(--def-bg);    color: var(--def); }
.badge-down { background: var(--surface-2); color: var(--text-3); }

/* BUTTONS */
.btn {
  display: inline-block; padding: 0.4rem 0.9rem; border-radius: 5px;
  font: inherit; font-size: var(--text-small); font-weight: 500;
  border: 1px solid var(--border-strong); background: var(--surface);
  color: var(--text); cursor: pointer; text-decoration: none; line-height: 1.3;
  transition: background 60ms linear, border-color 60ms linear;
}
.btn:hover:not(:disabled) { background: var(--surface-2); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-danger { color: var(--danger); border-color: color-mix(in srgb, var(--danger) 35%, var(--border)); }
.btn-danger:hover:not(:disabled) { background: var(--danger-bg); border-color: var(--danger); }
.btn-primary { color: var(--accent); border-color: color-mix(in srgb, var(--accent) 35%, var(--border)); }
.btn-primary:hover:not(:disabled) { background: color-mix(in srgb, var(--accent) 10%, var(--surface)); border-color: var(--accent); }
form.inline { display: inline; margin: 0; }

/* DANGER + SETTINGS — two-column row: label on the left (title + desc),
   control on the right. Title in body weight, desc in small muted copy. */
.danger-row, .settings-row, .settings-submit {
  display: flex; align-items: center; justify-content: space-between;
  gap: 1rem; padding: 0.875rem 1.125rem; border-top: 1px solid var(--border);
}
.danger-row:first-child, .settings-row:first-child { border-top: 0; }
.danger-row .label, .settings-row > label { flex: 1; min-width: 0; }
.danger-row .title, .settings-row > label > :first-child {
  font-weight: 600; color: var(--text); font-size: var(--text-body);
}
.danger-row .desc, .settings-row .hint {
  margin-top: 0.15rem; font-size: var(--text-small); color: var(--text-2);
  line-height: 1.45;
}
.settings-row .hint { color: var(--text-3); }
.settings-row input[type="number"],
.settings-row input[type="text"] {
  padding: 0.35rem 0.65rem;
  font: inherit; font-family: var(--font-mono);
  font-size: var(--text-small); background: var(--surface); color: var(--text);
  border: 1px solid var(--border-strong); border-radius: 4px;
  font-variant-numeric: tabular-nums;
}
.settings-row input[type="number"] { width: 8rem; }
.settings-row input[type="text"]   { width: 22rem; max-width: 100%; }
.settings-row input:focus { outline: none; border-color: var(--accent); box-shadow: 0 0 0 3px var(--ring); }
.settings-submit { background: var(--surface-2); padding: 0.875rem 1.125rem; }
.settings-submit .desc { font-size: var(--text-small); color: var(--text-2); }

/* FOOTER */
footer {
  margin-top: 2.5rem; padding-top: 1rem; border-top: 1px solid var(--border);
  text-align: center; color: var(--text-3); font-size: var(--text-small);
}
"#;
