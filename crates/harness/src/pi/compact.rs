//! What Zeron tells the session panel about Pi's auto-compaction: whether it
//! is on, where it starts, and whether it is running now. Published as the
//! host channel [`COMPACT_CHANNEL`](zeron_proto::extension_ui::COMPACT_CHANNEL).

use serde_json::Value;
use zeron_proto::extension_ui::CompactStatus;

#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct Compaction {
    started_ms: Option<u64>,
    reason: Option<String>,
    tokens_after: Option<u64>,
    error: Option<String>,
}

impl Compaction {
    /// Fold one Pi frame in; true when the status changed.
    pub fn frame(&mut self, frame: &Value, now_ms: u64) -> bool {
        let before = self.clone();
        match frame["type"].as_str().unwrap_or("") {
            "compaction_start" => {
                *self = Self {
                    started_ms: Some(now_ms),
                    reason: frame["reason"].as_str().map(str::to_owned),
                    ..Default::default()
                }
            }
            // Success, failure and abort all end here (an overflow failure
            // without a start too); an abort carries no error.
            "compaction_end" => {
                *self = Self {
                    tokens_after: frame["result"]["estimatedTokensAfter"].as_u64(),
                    error: frame["errorMessage"]
                        .as_str()
                        .filter(|error| !error.is_empty())
                        .map(str::to_owned),
                    ..Default::default()
                }
            }
            // The next measured reply supersedes what the compaction left.
            "message_end" if frame["message"]["role"] == "assistant" => self.tokens_after = None,
            _ => {}
        }
        *self != before
    }

    /// `get_state.isCompacting` is the truth when a frame was missed.
    pub fn sync(&mut self, is_compacting: Option<bool>, now_ms: u64) -> bool {
        match (is_compacting, self.started_ms) {
            (Some(false), Some(_)) => {
                self.started_ms = None;
                self.reason = None;
                true
            }
            (Some(true), None) => {
                self.started_ms = Some(now_ms);
                true
            }
            _ => false,
        }
    }

    pub fn status(&self, enabled: bool, threshold_tokens: Option<u64>) -> CompactStatus {
        CompactStatus {
            enabled,
            threshold_tokens,
            compacting: self.started_ms.is_some(),
            reason: self.reason.clone(),
            started_at_ms: self.started_ms,
            tokens_after: self.tokens_after,
            error: self.error.clone(),
        }
    }
}

/// Where Pi starts compacting: past `window - reserve` tokens. `None` when
/// the reserve leaves no room.
pub(super) fn threshold(window: u64, reserve: u64) -> Option<u64> {
    window.checked_sub(reserve).filter(|tokens| *tokens > 0)
}

pub(super) fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_threshold_is_the_window_less_the_reserve() {
        assert_eq!(threshold(200_000, 65_536), Some(134_464));
        assert_eq!(threshold(128_000, 16_384), Some(111_616));
        assert_eq!(threshold(200_000, 200_000), None);
        assert_eq!(threshold(8_000, 16_384), None);
    }

    #[test]
    fn frames_start_and_end_a_compaction() {
        let mut compaction = Compaction::default();
        assert!(compaction.frame(&json!({"type":"compaction_start","reason":"threshold"}), 5));
        let status = compaction.status(true, Some(100));
        assert!(status.compacting);
        assert_eq!(status.started_at_ms, Some(5));
        assert_eq!(status.reason.as_deref(), Some("threshold"));
        // A repeated start at the same moment changes nothing.
        assert!(!compaction.frame(&json!({"type":"compaction_start","reason":"threshold"}), 5));

        let end = json!({"type":"compaction_end","result":{"estimatedTokensAfter":9}});
        assert!(compaction.frame(&end, 6));
        let status = compaction.status(true, Some(100));
        assert!(!status.compacting);
        assert_eq!(status.tokens_after, Some(9));
        assert_eq!(status.error, None);

        // A user's message leaves the measurement; the next reply clears it.
        let user = json!({"type":"message_end","message":{"role":"user"}});
        assert!(!compaction.frame(&user, 7));
        let reply = json!({"type":"message_end","message":{"role":"assistant"}});
        assert!(compaction.frame(&reply, 8));
        assert_eq!(compaction.status(true, None).tokens_after, None);
        assert!(!compaction.frame(&json!({"type":"turn_end"}), 9));
    }

    #[test]
    fn a_failed_compaction_keeps_its_error_and_an_aborted_one_none() {
        let mut compaction = Compaction::default();
        compaction.frame(&json!({"type":"compaction_start"}), 1);
        let failed = json!({"type":"compaction_end","errorMessage":"too large"});
        assert!(compaction.frame(&failed, 2));
        assert_eq!(
            compaction.status(true, None).error.as_deref(),
            Some("too large")
        );

        compaction.frame(&json!({"type":"compaction_start"}), 3);
        let aborted = json!({"type":"compaction_end","aborted":true,"errorMessage":""});
        assert!(compaction.frame(&aborted, 4));
        let status = compaction.status(true, None);
        assert!(!status.compacting);
        assert_eq!(status.error, None);
        assert_eq!(status.tokens_after, None);
    }

    #[test]
    fn get_state_corrects_a_missed_frame() {
        let mut compaction = Compaction::default();
        assert!(!compaction.sync(Some(false), 1));
        assert!(compaction.sync(Some(true), 2));
        assert!(compaction.status(false, None).compacting);
        assert!(!compaction.sync(Some(true), 3));
        assert!(!compaction.sync(None, 3));
        assert!(compaction.sync(Some(false), 4));
        assert!(!compaction.status(false, None).compacting);
    }
}
