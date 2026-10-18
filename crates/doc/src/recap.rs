//! The cache key of a chat's recap, shared by the engine (which writes recaps)
//! and the UI (which decides whether one still describes the transcript).
//!
//! Both sides compute it over the same joined entry list (continuations folded
//! onto their root), so the key can never drift between them. The last entry's
//! id alone is not enough: a settled assistant entry still grows when a
//! continuation or a tool output lands on it, so the entry and part counts ride
//! along.

use serde::{Deserialize, Serialize};

use crate::SessionMessageEntry;

/// What a recap was generated from. Equal keys mean "nothing new since".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecapKey {
    pub last_entry_id: String,
    pub entry_count: usize,
    pub last_part_count: usize,
}

/// The key of a joined transcript; `None` for an empty one.
pub fn recap_key(entries: &[SessionMessageEntry]) -> Option<RecapKey> {
    let last = entries.last()?;
    Some(RecapKey {
        last_entry_id: last.id.clone(),
        entry_count: entries.len(),
        last_part_count: last.parts.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MessagePart, MessageRole};

    fn entry(id: &str, parts: usize) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts: (0..parts)
                .map(|n| MessagePart::Text {
                    id: format!("{id}-{n}"),
                    text: "x".into(),
                })
                .collect(),
            created_at: 0,
            device_id: "d".into(),
            status: None,
            continuation_of: None,
            duration_ms: None,
        }
    }

    #[test]
    fn key_tracks_growth_of_the_last_entry() {
        assert_eq!(recap_key(&[]), None);
        let before = recap_key(&[entry("a", 1), entry("b", 2)]).unwrap();
        assert_eq!(before.last_entry_id, "b");
        assert_eq!(before.entry_count, 2);
        // The same entry gaining a part (a continuation joined onto it).
        let grown = recap_key(&[entry("a", 1), entry("b", 3)]).unwrap();
        assert_ne!(before, grown);
        // A new entry after it.
        let appended = recap_key(&[entry("a", 1), entry("b", 2), entry("c", 1)]).unwrap();
        assert_ne!(before, appended);
        assert_eq!(before, recap_key(&[entry("a", 1), entry("b", 2)]).unwrap());
    }
}
