//! Inline env editor — renders only when `BRIDGE_ENV_FILE` is set.

use crate::inspect::Snapshot;
use crate::sanitize;

pub(super) fn build_settings_section(snapshot: &Snapshot) -> String {
    let Some(path) = snapshot.env_file.as_ref() else {
        return r#"<div class="panel"><div class="danger-row"><div class="label"><div class="title">Config editing disabled</div><div class="desc">Set <code>BRIDGE_ENV_FILE</code> to the env file path to enable in-place editing.</div></div></div></div>"#.to_string();
    };
    // Only offer the autostart-dir field when autostart is configured.
    let autostart_dir_row = snapshot.autostart.as_ref().map_or_else(String::new, |a| {
        format!(
            r#"<div class="settings-row">
    <label>
      <div>Autostart working directory</div>
      <div class="hint">Where <code>{cmd}</code> runs for the autostart session. Must exist.</div>
    </label>
    <input type="text" name="autostart_dir" value="{dir}" size="40" required>
  </div>"#,
            cmd = sanitize::escape_html(&a.command),
            dir = sanitize::escape_html(&a.dir),
        )
    });

    format!(
        r#"<div class="panel"><form method="POST" action="/actions/config" class="settings-form">
  <div class="settings-row">
    <label>
      <div>Long-poll timeout</div>
      <div class="hint">Seconds tebis waits for Telegram updates per request. 1–900.</div>
    </label>
    <input type="number" name="poll_timeout" min="1" max="900" value="{poll}" required>
  </div>
  <div class="settings-row">
    <label>
      <div>Max capture output chars</div>
      <div class="hint">Largest <code>/read</code> response before truncation. 100–20000.</div>
    </label>
    <input type="number" name="max_output_chars" min="100" max="20000" value="{max_chars}" required>
  </div>
  {autostart_dir_row}
  <div class="settings-submit">
    <div class="desc">Writes to <code>{path_html}</code> and restarts the bridge.</div>
    <button type="submit" class="btn btn-primary">Save &amp; restart</button>
  </div>
</form></div>"#,
        poll = snapshot.poll_timeout,
        max_chars = snapshot.max_output_chars,
        path_html = sanitize::escape_html(path),
    )
}
