//! Per-chat "last inbound was voice?" flag. Drives TTS for both the
//! synchronous handler reply and the async hook reply. Sticky: stays
//! at whatever the most recent inbound set it to (no ordering needed).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct VoicePref {
    flags: Mutex<HashMap<i64, bool>>,
}

impl VoicePref {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn set(&self, chat_id: i64, was_voice: bool) {
        let mut guard = match self.flags.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.insert(chat_id, was_voice);
    }

    /// Defaults to false (text mode) for unknown chats and poisoned mutex.
    #[must_use]
    pub fn last_was_voice(&self, chat_id: i64) -> bool {
        match self.flags.lock() {
            Ok(g) => g.get(&chat_id).copied().unwrap_or(false),
            Err(p) => p.into_inner().get(&chat_id).copied().unwrap_or(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_chat_defaults_to_text() {
        let pref = VoicePref::new();
        assert!(!pref.last_was_voice(42));
    }

    #[test]
    fn set_then_read_round_trips() {
        let pref = VoicePref::new();
        pref.set(7, true);
        assert!(pref.last_was_voice(7));
        pref.set(7, false);
        assert!(!pref.last_was_voice(7));
    }

    #[test]
    fn set_is_per_chat_keyed() {
        let pref = VoicePref::new();
        pref.set(1, true);
        pref.set(2, false);
        assert!(pref.last_was_voice(1));
        assert!(!pref.last_was_voice(2));
    }
}
