//! Recap: a few dim lines at the end of a chat's transcript telling someone who
//! just came back where it stands. The text is written on the chat's host by the
//! Thread-naming model (the `RecapChat` RPC); this module decides when to ask,
//! keeps what came back, and tells the transcript what to draw.
//!
//! Triggers: the window re-activates, or the chat is reopened, after at least
//! [`AWAY_SECS`] away and with settled assistant work the person has not seen
//! (and `/recap` on demand). Never in side chats or subagent tabs. The "left at"
//! facts are latched when the person leaves — before the chat is marked seen and
//! before its transcript is cleared — because by return time both are gone.
//! Dismissal is device-local: the host keeps only the generated text.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use chrono::{DateTime, Duration, Utc};
use gpui::Context;
use serde::Deserialize;
use zeron_doc::recap::{RecapKey, recap_key};
use zeron_doc::{MessageRole, MessageStatus, SessionMessageEntry};
use zeron_proto::ChatIndicator;
use zeron_rpc::methods;

use crate::state::AppState;

/// Time away that earns a recap.
const AWAY_SECS: i64 = 180;
/// Fewer entries than this is a conversation short enough to just read.
const MIN_ENTRIES: usize = 3;
/// After a failed or refused request, automatic ones for that chat wait.
const FAILURE_COOLDOWN_SECS: i64 = 600;
const MAX_LINES: usize = 3;

/// How long away is "away": three minutes, `ZERON_RECAP_AWAY_SECS` overrides.
pub(crate) fn away_threshold() -> Duration {
    static SECS: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    Duration::seconds(*SECS.get_or_init(|| {
        std::env::var("ZERON_RECAP_AWAY_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<i64>().ok())
            .filter(|secs| *secs >= 0)
            .unwrap_or(AWAY_SECS)
    }))
}

/// A recap that came back from the host.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RecapView {
    pub lines: Vec<String>,
    /// The transcript state it describes; anything newer hides it.
    pub key: RecapKey,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RecapPhase {
    /// A request is out. Only a `/recap` the person asked for shows it.
    Generating {
        manual: bool,
        token: u64,
    },
    Ready(RecapView),
    /// Closed by the person, for this transcript state only.
    Dismissed {
        key: RecapKey,
    },
    /// The host could not write one; automatic requests pause.
    Failed {
        at: DateTime<Utc>,
    },
}

/// When someone left a chat and what they had seen of it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Departure {
    /// The moment they left; for a chat reopened after a restart, only an
    /// upper bound (the synced seen marker). `None`: unknown.
    pub at: Option<DateTime<Utc>>,
    /// The transcript as they last saw it, when this device watched it.
    pub key: Option<RecapKey>,
    /// The chat had activity nobody had seen when it was reopened.
    pub unseen: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SkipReason {
    Disabled,
    Unsupported,
    NotReplayed,
    Busy,
    AwayTooShort,
    NothingNewSinceLeaving,
    TooFewEntries,
    InFlight,
    AlreadyHave,
    Dismissed,
    Cooldown,
}

impl SkipReason {
    /// The latched departure is worth keeping and re-examining later.
    pub(crate) fn keeps_pending(self) -> bool {
        self == Self::NotReplayed
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AutoPlan {
    Generate,
    Skip(SkipReason),
}

pub(crate) struct AutoInputs<'a> {
    pub enabled: bool,
    pub host_supports: bool,
    pub now: DateTime<Utc>,
    pub away_threshold: Duration,
    pub departure: &'a Departure,
    pub status: ChatIndicator,
    /// A message is on its way to the host (a sent echo or queued command).
    pub send_in_flight: bool,
    pub replayed: bool,
    pub entries: &'a [SessionMessageEntry],
    pub existing: Option<&'a RecapPhase>,
}

/// The transcript state a person leaves behind. A reply still streaming at
/// that moment is not "seen": it settling while they are away is exactly the
/// work a recap covers, even when it gains no new part, so the key names the
/// entry before it.
pub(crate) fn seen_key(entries: &[SessionMessageEntry]) -> Option<RecapKey> {
    match entries.split_last() {
        Some((last, rest)) if last.status == Some(MessageStatus::Streaming) => recap_key(rest),
        _ => recap_key(entries),
    }
}

fn settled_assistant(entry: &SessionMessageEntry) -> bool {
    entry.role == MessageRole::Assistant && entry.status != Some(MessageStatus::Streaming)
}

/// Did settled assistant work land after the transcript state `since`? With no
/// snapshot (a chat first opened on this device), unseen activity stands in.
pub(crate) fn new_settled_assistant_work(
    entries: &[SessionMessageEntry],
    since: Option<&RecapKey>,
    unseen: bool,
) -> bool {
    let Some(since) = since else {
        return unseen && entries.iter().any(settled_assistant);
    };
    let Some(at) = entries.iter().position(|e| e.id == since.last_entry_id) else {
        // History moved under us (a rewind or a reset): judge by what is there.
        return entries.iter().any(settled_assistant);
    };
    entries[at..].iter().enumerate().any(|(n, entry)| {
        if n == 0 {
            // The entry they saw can still have grown (a continuation joined it).
            settled_assistant(entry) && entry.parts.len() > since.last_part_count
        } else {
            settled_assistant(entry)
        }
    })
}

/// Whether returning to a chat should ask for a recap, and if not, why.
pub(crate) fn plan_auto(input: &AutoInputs) -> AutoPlan {
    use SkipReason::*;
    let skip = |reason| AutoPlan::Skip(reason);
    if !input.enabled {
        return skip(Disabled);
    }
    if !input.host_supports {
        return skip(Unsupported);
    }
    if !input.replayed {
        return skip(NotReplayed);
    }
    // Watching a live run needs no recap; waiting on the person does.
    if input.status == ChatIndicator::Working || input.send_in_flight {
        return skip(Busy);
    }
    let away_long_enough = match input.departure.at {
        Some(at) => input.now.signed_duration_since(at) >= input.away_threshold,
        None => input.departure.unseen,
    };
    if !away_long_enough {
        return skip(AwayTooShort);
    }
    if !new_settled_assistant_work(
        input.entries,
        input.departure.key.as_ref(),
        input.departure.unseen,
    ) {
        return skip(NothingNewSinceLeaving);
    }
    if input.entries.len() < MIN_ENTRIES {
        return skip(TooFewEntries);
    }
    let key = recap_key(input.entries);
    match input.existing {
        Some(RecapPhase::Generating { .. }) => skip(InFlight),
        Some(RecapPhase::Ready(view)) if Some(&view.key) == key.as_ref() => skip(AlreadyHave),
        Some(RecapPhase::Dismissed { key: closed }) if Some(closed) == key.as_ref() => {
            skip(Dismissed)
        }
        Some(RecapPhase::Failed { at })
            if input.now.signed_duration_since(*at) < Duration::seconds(FAILURE_COOLDOWN_SECS) =>
        {
            skip(Cooldown)
        }
        _ => AutoPlan::Generate,
    }
}

/// Whether the recap row is drawn: only while it still describes the end of the
/// transcript, and never over a live run or a message still on its way.
pub(crate) fn recap_visible(
    phase: Option<&RecapPhase>,
    current_key: Option<&RecapKey>,
    status: ChatIndicator,
    echoes_pending: bool,
) -> bool {
    if echoes_pending || status == ChatIndicator::Working {
        return false;
    }
    match phase {
        Some(RecapPhase::Ready(view)) => current_key == Some(&view.key),
        Some(RecapPhase::Generating { manual, .. }) => *manual,
        _ => false,
    }
}

/// Up to three trimmed, non-empty lines.
pub(crate) fn recap_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(MAX_LINES)
        .map(str::to_owned)
        .collect()
}

/// What the transcript draws.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RecapRow {
    pub lines: Vec<String>,
    pub generating: bool,
    /// Changes whenever what is drawn does (never 0).
    pub version: u64,
}

impl RecapRow {
    fn ready(lines: Vec<String>) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        lines.hash(&mut hasher);
        Self {
            lines,
            generating: false,
            version: hasher.finish() | 1,
        }
    }

    fn generating() -> Self {
        Self {
            lines: Vec::new(),
            generating: true,
            version: 2,
        }
    }
}

/// `RecapChat`'s reply.
#[derive(Debug, Deserialize)]
pub(crate) struct Outcome {
    pub status: String,
    #[serde(default)]
    pub recap: Option<WireRecap>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireRecap {
    pub text: String,
    pub key: RecapKey,
}

/// What to tell the person after a request finished.
#[derive(Debug, PartialEq)]
pub(crate) enum Notice {
    None,
    Info(String),
    Error(String),
}

/// Per-device recap state: phases by chat, the latched departures, the request
/// token counter. Pure — the `AppState` methods below add the RPC and the UI.
#[derive(Default)]
pub(crate) struct RecapStore {
    by_chat: HashMap<String, RecapPhase>,
    left: HashMap<String, Departure>,
    pending_open: Option<(String, Departure)>,
    next_token: u64,
}

impl RecapStore {
    pub(crate) fn phase(&self, chat_id: &str) -> Option<&RecapPhase> {
        self.by_chat.get(chat_id)
    }

    pub(crate) fn pending(&self) -> Option<&(String, Departure)> {
        self.pending_open.as_ref()
    }

    pub(crate) fn clear_pending(&mut self) {
        self.pending_open = None;
    }

    /// The person leaves `chat_id` (another chat opens, or the window loses
    /// focus) with the transcript as `key` describes it.
    pub(crate) fn note_left(&mut self, chat_id: &str, at: DateTime<Utc>, key: Option<RecapKey>) {
        self.left.insert(
            chat_id.to_owned(),
            Departure {
                at: Some(at),
                key,
                unseen: false,
            },
        );
    }

    /// `chat_id` was reopened. Must run before it is marked seen: only a chat
    /// with activity nobody saw can be owed a recap.
    pub(crate) fn latch_open(
        &mut self,
        chat_id: &str,
        unseen: bool,
        last_seen_at: Option<DateTime<Utc>>,
    ) {
        let left = self.left.remove(chat_id);
        if !unseen {
            return;
        }
        let departure = match left {
            Some(left) => Departure {
                unseen: true,
                ..left
            },
            None => Departure {
                at: last_seen_at,
                key: None,
                unseen: true,
            },
        };
        self.pending_open = Some((chat_id.to_owned(), departure));
    }

    /// The window is back on `chat_id`.
    pub(crate) fn note_returned(&mut self, chat_id: &str) {
        if let Some(departure) = self.left.remove(chat_id) {
            self.pending_open = Some((chat_id.to_owned(), departure));
        }
    }

    /// A request goes out; the returned token identifies its reply.
    pub(crate) fn begin(&mut self, chat_id: &str, manual: bool) -> u64 {
        self.next_token = self.next_token.wrapping_add(1);
        let token = self.next_token;
        self.by_chat
            .insert(chat_id.to_owned(), RecapPhase::Generating { manual, token });
        token
    }

    /// The reply to request `token`. A reply to anything but the latest request
    /// for the chat, or one that no longer describes the transcript, is dropped.
    pub(crate) fn finish(
        &mut self,
        chat_id: &str,
        token: u64,
        result: Result<Outcome, String>,
        current_key: Option<&RecapKey>,
        now: DateTime<Utc>,
    ) -> Notice {
        let Some(RecapPhase::Generating {
            manual,
            token: live,
        }) = self.by_chat.get(chat_id).cloned()
        else {
            return Notice::None;
        };
        if live != token {
            return Notice::None;
        }
        let failed = RecapPhase::Failed { at: now };
        let (phase, notice) = match result {
            Ok(Outcome {
                recap: Some(recap), ..
            }) => {
                if current_key.is_some_and(|key| *key != recap.key) {
                    let notice = if manual {
                        Notice::Info(
                            "The conversation moved on while the recap was written.".into(),
                        )
                    } else {
                        Notice::None
                    };
                    (None, notice)
                } else {
                    let lines = recap_lines(&recap.text);
                    if lines.is_empty() {
                        (Some(failed), Notice::None)
                    } else {
                        (
                            Some(RecapPhase::Ready(RecapView {
                                lines,
                                key: recap.key,
                            })),
                            Notice::None,
                        )
                    }
                }
            }
            Ok(Outcome { status, .. }) if status == "nothing-to-recap" => {
                let notice = if manual {
                    Notice::Info("Nothing to recap yet.".into())
                } else {
                    Notice::None
                };
                (None, notice)
            }
            // The host paused this chat after a failure, or had nothing stored.
            Ok(_) => (Some(failed), Notice::None),
            Err(error) => {
                let notice = if manual {
                    Notice::Error(format!("Recap failed: {error}"))
                } else {
                    Notice::None
                };
                (Some(failed), notice)
            }
        };
        match phase {
            Some(phase) => {
                self.by_chat.insert(chat_id.to_owned(), phase);
            }
            None => {
                self.by_chat.remove(chat_id);
            }
        }
        notice
    }

    /// Close the recap shown for `chat_id`; it stays closed for that
    /// transcript state and returns with the next one.
    pub(crate) fn dismiss(&mut self, chat_id: &str) -> bool {
        let Some(RecapPhase::Ready(view)) = self.by_chat.get(chat_id) else {
            return false;
        };
        let key = view.key.clone();
        self.by_chat
            .insert(chat_id.to_owned(), RecapPhase::Dismissed { key });
        true
    }
}

impl AppState {
    /// `chat_id` was just selected, before it is marked seen.
    pub(crate) fn note_chat_opened(&mut self, chat_id: &str) {
        let Some(chat) = self.chats.iter().find(|chat| chat.id == chat_id) else {
            return;
        };
        let (unseen, last_seen_at) = (chat.unseen(), chat.last_seen_at);
        self.recaps.latch_open(chat_id, unseen, last_seen_at);
    }

    /// The window lost focus: the selected chat is what the person left.
    pub(crate) fn note_window_blurred(&mut self, now: DateTime<Utc>) {
        if let Some(chat_id) = self.selected_chat.clone()
            && self.transcript_replayed
        {
            let key = seen_key(&self.transcript);
            self.recaps.note_left(&chat_id, now, key);
        }
    }

    /// The window is focused again.
    pub(crate) fn note_window_returned(&mut self) {
        if let Some(chat_id) = self.selected_chat.clone() {
            self.recaps.note_returned(&chat_id);
        }
    }

    /// Once per frame while a departure is latched: ask for a recap if it
    /// qualifies. Cheap when nothing is latched.
    pub(crate) fn recap_tick(&mut self, now: DateTime<Utc>, enabled: bool, cx: &mut Context<Self>) {
        let Some((chat_id, departure)) = self.recaps.pending().cloned() else {
            return;
        };
        if self.selected_chat.as_deref() != Some(chat_id.as_str()) {
            self.recaps.clear_pending();
            return;
        }
        let Some(chat) = self.chats.iter().find(|chat| chat.id == chat_id) else {
            self.recaps.clear_pending();
            return;
        };
        let plan = plan_auto(&AutoInputs {
            enabled,
            host_supports: self.chat_host_supports(&chat_id, zeron_proto::capabilities::RECAP_V1)
                && chat.has_returning_reader(&self.chats),
            now,
            away_threshold: away_threshold(),
            departure: &departure,
            status: self.display_status_for(chat, now),
            send_in_flight: self.send_pending(&chat_id, now) || !self.pending_echoes().is_empty(),
            replayed: self.transcript_replayed,
            entries: &self.transcript,
            existing: self.recaps.phase(&chat_id),
        });
        match plan {
            AutoPlan::Skip(reason) if reason.keeps_pending() => {}
            AutoPlan::Skip(_) => self.recaps.clear_pending(),
            AutoPlan::Generate => {
                self.recaps.clear_pending();
                self.request_recap(&chat_id, false, cx);
            }
        }
    }

    /// Ask `chat_id`'s host for a recap. `manual` is `/recap`: it shows
    /// progress, regenerates a current recap, and reports problems.
    pub(crate) fn request_recap(&mut self, chat_id: &str, manual: bool, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.iter().find(|chat| chat.id == chat_id) else {
            return;
        };
        if !chat.has_returning_reader(&self.chats) {
            if manual {
                crate::toast::info(cx, "Recaps are for main conversations.");
            }
            return;
        }
        let device = chat.device_id.clone();
        let status = self.display_status_for(chat, Utc::now());
        if manual && status == ChatIndicator::Working {
            crate::toast::info(cx, "The agent is still working. Nothing to recap yet.");
            return;
        }
        let Some(handle) = self.engine().cloned() else {
            if manual {
                crate::toast::error(cx, "Engine not connected");
            }
            return;
        };
        if !self.chat_host_supports(chat_id, zeron_proto::capabilities::RECAP_V1) {
            if manual {
                crate::toast::info(cx, "Update that device's Zeron engine to use recaps.");
            }
            return;
        }
        let token = self.recaps.begin(chat_id, manual);
        if manual {
            cx.notify();
        }
        let chat_id = chat_id.to_owned();
        let params = serde_json::json!({
            "chatId": chat_id,
            "action": "generate",
            "force": manual,
            "targetDeviceId": device,
        });
        cx.spawn(async move |this, cx| {
            let result = handle
                .client()
                .call(methods::RECAP_CHAT, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Outcome>(value).map_err(|error| error.to_string())
                });
            this.update(cx, |state, cx| {
                state.finish_recap(&chat_id, token, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_recap(
        &mut self,
        chat_id: &str,
        token: u64,
        result: Result<Outcome, String>,
        cx: &mut Context<Self>,
    ) {
        let current_key = (self.selected_chat.as_deref() == Some(chat_id)
            && self.transcript_replayed)
            .then(|| recap_key(&self.transcript))
            .flatten();
        let notice = self
            .recaps
            .finish(chat_id, token, result, current_key.as_ref(), Utc::now());
        match notice {
            Notice::None => {}
            Notice::Info(message) => crate::toast::info(cx, message),
            Notice::Error(message) => crate::toast::error(cx, message),
        }
        cx.notify();
    }

    /// The recap row's close button.
    pub(crate) fn dismiss_recap(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        if self.recaps.dismiss(chat_id) {
            cx.notify();
        }
    }

    /// The recap row to draw at the end of `chat_id`'s transcript, if any.
    pub(crate) fn recap_row(&self, chat_id: &str, now: DateTime<Utc>) -> Option<RecapRow> {
        if self.selected_chat.as_deref() != Some(chat_id) || !self.transcript_replayed {
            return None;
        }
        let phase = self.recaps.phase(chat_id)?;
        let chat = self.chats.iter().find(|chat| chat.id == chat_id)?;
        let echoes_pending = !self.pending_echoes().is_empty() || self.send_pending(chat_id, now);
        let key = recap_key(&self.transcript);
        if !recap_visible(
            Some(phase),
            key.as_ref(),
            self.display_status_for(chat, now),
            echoes_pending,
        ) {
            return None;
        }
        Some(match phase {
            RecapPhase::Ready(view) => RecapRow::ready(view.lines.clone()),
            _ => RecapRow::generating(),
        })
    }
}

/// A chat row last seen an hour before `now()`, for tests here and in the
/// transcript.
#[cfg(test)]
pub(crate) fn test_chat(id: &str, last_message_at: Option<DateTime<Utc>>) -> zeron_proto::Chat {
    zeron_proto::Chat {
        id: id.into(),
        device_id: "dev".into(),
        title: None,
        archived: false,
        cwd: None,
        branch: None,
        checkout_id: None,
        source_context: None,
        config: None,
        last_message_preview: None,
        last_message_at,
        created_at: tests::now() - Duration::days(1),
        harness_session_id: None,
        harness_session_cwd: None,
        parent_chat_id: None,
        space_id: None,
        last_seen_at: Some(tests::now() - Duration::hours(1)),
        room_gen: None,
    }
}

#[cfg(test)]
mod tests {
    use super::test_chat as chat;
    use super::*;
    use zeron_doc::MessagePart;

    fn entry(id: &str, role: MessageRole, status: MessageStatus) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts: vec![MessagePart::Text {
                id: format!("{id}-t"),
                text: id.into(),
            }],
            created_at: 1,
            device_id: "d".into(),
            status: Some(status),
            continuation_of: None,
            duration_ms: None,
        }
    }

    /// user, assistant, user, assistant, ...: `n` settled entries.
    fn transcript(n: usize) -> Vec<SessionMessageEntry> {
        (0..n)
            .map(|i| {
                let role = if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                };
                entry(&format!("e{i}"), role, MessageStatus::Complete)
            })
            .collect()
    }

    pub(super) fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc()
    }

    fn view(entries: &[SessionMessageEntry]) -> RecapView {
        RecapView {
            lines: vec!["Goal".into()],
            key: recap_key(entries).unwrap(),
        }
    }

    /// Left at `now - away`, having seen the first `seen` entries.
    fn departure(away: Duration, seen: &[SessionMessageEntry]) -> Departure {
        Departure {
            at: Some(now() - away),
            key: recap_key(seen),
            unseen: false,
        }
    }

    struct Case {
        entries: Vec<SessionMessageEntry>,
        departure: Departure,
        status: ChatIndicator,
        existing: Option<RecapPhase>,
        enabled: bool,
        supported: bool,
        replayed: bool,
        sending: bool,
    }

    fn base() -> Case {
        let entries = transcript(6);
        Case {
            departure: departure(Duration::minutes(5), &entries[..4]),
            entries,
            status: ChatIndicator::Idle,
            existing: None,
            enabled: true,
            supported: true,
            replayed: true,
            sending: false,
        }
    }

    fn plan(case: &Case) -> AutoPlan {
        plan_auto(&AutoInputs {
            enabled: case.enabled,
            host_supports: case.supported,
            now: now(),
            away_threshold: Duration::seconds(AWAY_SECS),
            departure: &case.departure,
            status: case.status,
            send_in_flight: case.sending,
            replayed: case.replayed,
            entries: &case.entries,
            existing: case.existing.as_ref(),
        })
    }

    fn skipped(reason: SkipReason) -> AutoPlan {
        AutoPlan::Skip(reason)
    }

    #[test]
    fn plan_auto_walks_every_gate() {
        assert_eq!(plan(&base()), AutoPlan::Generate);

        let mut case = base();
        case.enabled = false;
        assert_eq!(plan(&case), skipped(SkipReason::Disabled));

        let mut case = base();
        case.supported = false;
        assert_eq!(plan(&case), skipped(SkipReason::Unsupported));

        let mut case = base();
        case.replayed = false;
        assert_eq!(plan(&case), skipped(SkipReason::NotReplayed));
        assert!(SkipReason::NotReplayed.keeps_pending());
        assert!(!SkipReason::Busy.keeps_pending());

        // A run in progress is watched, not recapped; a question waiting for
        // the person is exactly what a recap is for.
        let mut case = base();
        case.status = ChatIndicator::Working;
        assert_eq!(plan(&case), skipped(SkipReason::Busy));
        case.status = ChatIndicator::AwaitingInput;
        assert_eq!(plan(&case), AutoPlan::Generate);
        let mut case = base();
        case.sending = true;
        assert_eq!(plan(&case), skipped(SkipReason::Busy));

        let mut case = base();
        case.departure.at = Some(now() - Duration::seconds(AWAY_SECS - 1));
        assert_eq!(plan(&case), skipped(SkipReason::AwayTooShort));
        case.departure.at = Some(now() - Duration::seconds(AWAY_SECS));
        assert_eq!(plan(&case), AutoPlan::Generate);

        // Nothing settled landed since the person left.
        let mut case = base();
        case.departure = departure(Duration::minutes(5), &case.entries);
        assert_eq!(plan(&case), skipped(SkipReason::NothingNewSinceLeaving));

        let mut case = base();
        case.entries.truncate(2);
        case.departure = departure(Duration::minutes(5), &case.entries[..1]);
        assert_eq!(plan(&case), skipped(SkipReason::TooFewEntries));
    }

    #[test]
    fn plan_auto_respects_what_it_already_has() {
        let mut case = base();
        case.existing = Some(RecapPhase::Generating {
            manual: false,
            token: 1,
        });
        assert_eq!(plan(&case), skipped(SkipReason::InFlight));

        let mut case = base();
        case.existing = Some(RecapPhase::Ready(view(&case.entries)));
        assert_eq!(plan(&case), skipped(SkipReason::AlreadyHave));
        // A recap of older state does not count.
        case.existing = Some(RecapPhase::Ready(view(&case.entries[..4])));
        assert_eq!(plan(&case), AutoPlan::Generate);

        let mut case = base();
        case.existing = Some(RecapPhase::Dismissed {
            key: recap_key(&case.entries).unwrap(),
        });
        assert_eq!(plan(&case), skipped(SkipReason::Dismissed));
        case.existing = Some(RecapPhase::Dismissed {
            key: recap_key(&case.entries[..4]).unwrap(),
        });
        assert_eq!(
            plan(&case),
            AutoPlan::Generate,
            "a newer state recaps again"
        );

        let mut case = base();
        case.existing = Some(RecapPhase::Failed {
            at: now() - Duration::seconds(FAILURE_COOLDOWN_SECS - 1),
        });
        assert_eq!(plan(&case), skipped(SkipReason::Cooldown));
        case.existing = Some(RecapPhase::Failed {
            at: now() - Duration::seconds(FAILURE_COOLDOWN_SECS),
        });
        assert_eq!(plan(&case), AutoPlan::Generate);
    }

    #[test]
    fn unseen_activity_stands_in_when_no_snapshot_exists() {
        let mut case = base();
        // First open since a restart: only the synced seen marker is known.
        case.departure = Departure {
            at: Some(now() - Duration::hours(2)),
            key: None,
            unseen: true,
        };
        assert_eq!(plan(&case), AutoPlan::Generate);
        case.departure.unseen = false;
        assert_eq!(plan(&case), skipped(SkipReason::NothingNewSinceLeaving));
        // Never seen at all: unknown time counts as away when unseen.
        case.departure = Departure {
            at: None,
            key: None,
            unseen: true,
        };
        assert_eq!(plan(&case), AutoPlan::Generate);
    }

    #[test]
    fn new_work_sees_growth_and_unmatched_history() {
        let entries = transcript(4);
        let seen = recap_key(&entries).unwrap();
        assert!(!new_settled_assistant_work(&entries, Some(&seen), false));
        // The last entry grew a part.
        let mut grown = entries.clone();
        grown[3].parts.push(MessagePart::Text {
            id: "more".into(),
            text: "more".into(),
        });
        assert!(new_settled_assistant_work(&grown, Some(&seen), false));
        // An unsettled assistant entry is not settled work.
        let mut streaming = entries.clone();
        streaming.push(entry("e4", MessageRole::User, MessageStatus::Complete));
        streaming.push(entry(
            "e5",
            MessageRole::Assistant,
            MessageStatus::Streaming,
        ));
        assert!(!new_settled_assistant_work(&streaming, Some(&seen), false));
        // The remembered entry is gone (rewind): judge by what is there.
        let rewound = transcript(2);
        assert!(new_settled_assistant_work(&rewound, Some(&seen), false));
    }

    #[test]
    fn a_reply_settling_while_away_is_new_work() {
        let mut entries = transcript(4);
        entries[3].status = Some(MessageStatus::Streaming);
        let seen = seen_key(&entries).unwrap();
        assert_eq!(seen.last_entry_id, "e2", "the streaming reply was not seen");
        // The same single-part reply finishing, with nothing else added.
        entries[3].status = Some(MessageStatus::Complete);
        assert!(new_settled_assistant_work(&entries, Some(&seen), false));
        // A settled tail is seen as it is.
        assert_eq!(seen_key(&entries), recap_key(&entries));
        assert_eq!(seen_key(&[]), None);
    }

    #[test]
    fn recap_visible_follows_the_end_of_the_transcript() {
        let entries = transcript(4);
        let ready = RecapPhase::Ready(view(&entries));
        let key = recap_key(&entries);
        let idle = ChatIndicator::Idle;
        assert!(recap_visible(Some(&ready), key.as_ref(), idle, false));
        let newer = recap_key(&transcript(5));
        assert!(!recap_visible(Some(&ready), newer.as_ref(), idle, false));
        assert!(!recap_visible(
            Some(&ready),
            key.as_ref(),
            ChatIndicator::Working,
            false
        ));
        assert!(recap_visible(
            Some(&ready),
            key.as_ref(),
            ChatIndicator::AwaitingInput,
            false
        ));
        assert!(!recap_visible(Some(&ready), key.as_ref(), idle, true));
        let dismissed = RecapPhase::Dismissed {
            key: key.clone().unwrap(),
        };
        assert!(!recap_visible(Some(&dismissed), key.as_ref(), idle, false));
        let manual = RecapPhase::Generating {
            manual: true,
            token: 1,
        };
        let auto = RecapPhase::Generating {
            manual: false,
            token: 2,
        };
        assert!(recap_visible(Some(&manual), key.as_ref(), idle, false));
        assert!(!recap_visible(Some(&auto), key.as_ref(), idle, false));
        assert!(!recap_visible(None, key.as_ref(), idle, false));
    }

    #[test]
    fn recap_lines_keep_three_plain_lines() {
        assert_eq!(
            recap_lines("  one \n\n two\nthree\nfour"),
            vec!["one", "two", "three"]
        );
        assert!(recap_lines(" \n ").is_empty());
    }

    fn outcome(text: &str, entries: &[SessionMessageEntry]) -> Result<Outcome, String> {
        Ok(Outcome {
            status: "ready".into(),
            recap: Some(WireRecap {
                text: text.into(),
                key: recap_key(entries).unwrap(),
            }),
        })
    }

    #[test]
    fn late_response_is_discarded() {
        let entries = transcript(4);
        let key = recap_key(&entries);
        let mut store = RecapStore::default();

        // A reply to a superseded request changes nothing.
        let first = store.begin("c", false);
        let second = store.begin("c", true);
        assert_eq!(
            store.finish("c", first, outcome("old", &entries), key.as_ref(), now()),
            Notice::None
        );
        assert!(matches!(
            store.phase("c"),
            Some(RecapPhase::Generating { token, .. }) if *token == second
        ));

        // The transcript moved on: the recap no longer describes it.
        let newer = recap_key(&transcript(6));
        let notice = store.finish(
            "c",
            second,
            outcome("late", &entries),
            newer.as_ref(),
            now(),
        );
        assert!(matches!(notice, Notice::Info(_)), "manual says so");
        assert!(store.phase("c").is_none());

        // An automatic request drops it silently.
        let third = store.begin("c", false);
        let notice = store.finish("c", third, outcome("late", &entries), newer.as_ref(), now());
        assert_eq!(notice, Notice::None);
        assert!(store.phase("c").is_none());

        // The matching reply lands.
        let fourth = store.begin("c", false);
        store.finish(
            "c",
            fourth,
            outcome("Goal\nDone", &entries),
            key.as_ref(),
            now(),
        );
        let Some(RecapPhase::Ready(view)) = store.phase("c") else {
            panic!("expected a ready recap");
        };
        assert_eq!(view.lines, vec!["Goal", "Done"]);
    }

    #[test]
    fn failures_pause_automatic_requests_and_report_manual_ones() {
        let mut store = RecapStore::default();
        let auto = store.begin("c", false);
        assert_eq!(
            store.finish("c", auto, Err("boom".into()), None, now()),
            Notice::None
        );
        assert_eq!(store.phase("c"), Some(&RecapPhase::Failed { at: now() }));
        let manual = store.begin("c", true);
        assert_eq!(
            store.finish("c", manual, Err("boom".into()), None, now()),
            Notice::Error("Recap failed: boom".into())
        );
        let manual = store.begin("c", true);
        let nothing = Ok(Outcome {
            status: "nothing-to-recap".into(),
            recap: None,
        });
        assert!(matches!(
            store.finish("c", manual, nothing, None, now()),
            Notice::Info(_)
        ));
        assert!(store.phase("c").is_none());
    }

    #[test]
    fn dismiss_is_local_and_sticky_for_the_same_state_only() {
        let entries = transcript(4);
        let mut store = RecapStore::default();
        assert!(!store.dismiss("c"), "nothing to dismiss");
        let token = store.begin("c", false);
        store.finish(
            "c",
            token,
            outcome("Goal", &entries),
            recap_key(&entries).as_ref(),
            now(),
        );
        assert!(store.dismiss("c"));
        let key = recap_key(&entries);
        assert_eq!(
            store.phase("c"),
            Some(&RecapPhase::Dismissed {
                key: key.clone().unwrap()
            })
        );
        assert!(!recap_visible(
            store.phase("c"),
            key.as_ref(),
            ChatIndicator::Idle,
            false
        ));
        assert!(!store.dismiss("c"), "already closed");
    }

    #[test]
    fn departures_latch_before_the_chat_is_marked_seen() {
        let entries = transcript(4);
        let mut store = RecapStore::default();
        let left_at = now() - Duration::minutes(10);

        // Switched away from "c" with its transcript snapshotted, then back.
        store.note_left("c", left_at, recap_key(&entries));
        store.latch_open("c", true, Some(left_at - Duration::minutes(30)));
        let (chat, departure) = store.pending().cloned().unwrap();
        assert_eq!(chat, "c");
        assert_eq!(departure.at, Some(left_at), "the real departure wins");
        assert_eq!(departure.key, recap_key(&entries));
        assert!(departure.unseen);

        // A seen chat latches nothing.
        store.clear_pending();
        store.latch_open("d", false, None);
        assert!(store.pending().is_none());

        // A chat never visited here falls back to the synced marker.
        store.latch_open("e", true, Some(left_at));
        let (_, departure) = store.pending().cloned().unwrap();
        assert_eq!(departure.at, Some(left_at));
        assert_eq!(departure.key, None);

        // The window returning latches what the blur snapshot recorded.
        store.clear_pending();
        store.note_left("c", left_at, recap_key(&entries));
        store.note_returned("c");
        assert_eq!(store.pending().map(|(chat, _)| chat.as_str()), Some("c"));
        store.clear_pending();
        store.note_returned("c");
        assert!(store.pending().is_none(), "the blur snapshot is consumed");
    }

    #[test]
    fn note_chat_opened_latches_only_unseen_chats() {
        let mut state = AppState::new();
        state.chats.push(chat("unseen", Some(now())));
        state
            .chats
            .push(chat("seen", Some(now() - Duration::hours(2))));
        state.note_chat_opened("seen");
        assert!(state.recaps.pending().is_none());
        state.note_chat_opened("unseen");
        let (id, departure) = state.recaps.pending().cloned().unwrap();
        assert_eq!(id, "unseen");
        assert_eq!(departure.at, Some(now() - Duration::hours(1)));
        assert!(departure.unseen);
        // A chat the list does not know yet latches nothing.
        state.recaps.clear_pending();
        state.note_chat_opened("ghost");
        assert!(state.recaps.pending().is_none());
    }

    #[test]
    fn window_blur_snapshots_the_selected_transcript() {
        let mut state = AppState::new();
        state.chats.push(chat("c", Some(now())));
        state.selected_chat = Some("c".into());
        state.transcript = transcript(4);
        // Before the transcript replays there is nothing to snapshot.
        state.note_window_blurred(now());
        state.note_window_returned();
        assert!(state.recaps.pending().is_none());
        state.transcript_replayed = true;
        state.note_window_blurred(now() - Duration::minutes(9));
        state.note_window_returned();
        let (_, departure) = state.recaps.pending().cloned().unwrap();
        assert_eq!(departure.at, Some(now() - Duration::minutes(9)));
        assert_eq!(departure.key, recap_key(&state.transcript));
    }

    #[test]
    fn recap_row_tracks_visibility_and_changes_its_version() {
        let mut state = AppState::new();
        state
            .chats
            .push(chat("c", Some(now() - Duration::hours(2))));
        state.selected_chat = Some("c".into());
        state.transcript = transcript(4);
        state.transcript_replayed = true;
        assert!(state.recap_row("c", now()).is_none());

        let token = state.recaps.begin("c", true);
        let generating = state.recap_row("c", now()).expect("manual progress shows");
        assert!(generating.generating);

        let key = recap_key(&state.transcript);
        state.recaps.finish(
            "c",
            token,
            outcome("Goal\nDone", &state.transcript),
            key.as_ref(),
            now(),
        );
        let ready = state.recap_row("c", now()).expect("ready recap shows");
        assert_eq!(ready.lines, vec!["Goal", "Done"]);
        assert_ne!(ready.version, generating.version);
        assert_ne!(ready.version, 0);

        // Not for another chat, and not once something newer lands.
        assert!(state.recap_row("other", now()).is_none());
        state
            .transcript
            .push(entry("e4", MessageRole::User, MessageStatus::Complete));
        assert!(state.recap_row("c", now()).is_none());
        state.transcript.pop();
        assert!(state.recap_row("c", now()).is_some());

        // Dismissing hides it; a sent message still on its way hides it too.
        state.push_echo(
            "c",
            entry("echo", MessageRole::User, MessageStatus::Complete),
        );
        assert!(state.recap_row("c", now()).is_none());
        state.remove_echo("c", "echo");
        assert!(state.recap_row("c", now()).is_some());
        assert!(state.recaps.dismiss("c"));
        assert!(state.recap_row("c", now()).is_none());
    }
}
