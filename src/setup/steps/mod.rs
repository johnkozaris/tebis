//! Interactive wizard steps.
//!
//! One module per step. Submodules are private; public surface is
//! the `step_*` functions re-exported here for `super::run()` to
//! call in sequence.

mod autostart;
mod bot_token;
mod hooks;
mod inspect;
mod sessions;
mod tts;
mod user_id;
mod voice;

pub(super) use autostart::step_autostart;
pub(super) use bot_token::step_bot_token;
pub(super) use hooks::step_hooks_mode;
pub(super) use inspect::step_inspect_port;
pub(super) use sessions::step_session_allowlist;
pub(super) use tts::step_tts;
pub(super) use user_id::step_user_id;
pub(super) use voice::step_voice;
