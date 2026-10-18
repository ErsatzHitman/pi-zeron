//! Frontend-agnostic view logic: the derivations every viewport needs and none
//! of them should own — sort orders, staleness gating, sidebar grouping, the
//! boot gate, relative times.
//!
//! This lives in `proto` rather than in the viewport crate so the rules stay
//! pure and independently testable: the same workspace doc must produce the
//! same row order on every surface, and there is exactly one implementation
//! and one test suite per rule.
//!
//! Everything in this module is pure. `chat_indicator` (the status derivation
//! these gate on) is in [`crate::entities`].

use chrono::{DateTime, Utc};

use crate::{
    AuthState, Chat, ChatIndicator, RetryState, RunningSubagent, Session, SessionStatus, Space,
    WorkspaceScope,
};

// ---------------------------------------------------------------------------
// Connection + status
// ---------------------------------------------------------------------------

/// Viewport ⇄ engine connection lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connecting,
    Ready,
    Failed(String),
}

/// What a chat's status dot / working indicator should show right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indicator {
    None,
    Working,
    AwaitingInput,
    Errored,
}

/// A `Working`/`AwaitingInput` session older than this is treated as dead — a
/// crashed backend must never show an eternal "Working" (feature-inventory
/// §1.12). Engines heartbeat sessions well inside this window.
pub const SESSION_STALE_MS: i64 = 45_000;

/// Staleness-checked indicator for a session row. Pure.
pub fn effective_indicator(session: Option<&Session>, now: DateTime<Utc>) -> Indicator {
    let Some(session) = session else {
        return Indicator::None;
    };
    match session.status {
        SessionStatus::Idle => Indicator::None,
        SessionStatus::Errored => Indicator::Errored,
        SessionStatus::Working | SessionStatus::AwaitingInput => {
            let age_ms = now
                .signed_duration_since(session.updated_at)
                .num_milliseconds();
            if age_ms > SESSION_STALE_MS {
                Indicator::None
            } else if session.status == SessionStatus::Working {
                Indicator::Working
            } else {
                Indicator::AwaitingInput
            }
        }
    }
}

/// Subagents running under a chat, staleness-checked like
/// [`effective_indicator`]: a session row nobody has refreshed inside
/// [`SESSION_STALE_MS`] belongs to a dead engine and counts nothing. Pure.
pub fn running_subagents(session: Option<&Session>, now: DateTime<Utc>) -> u32 {
    session
        .filter(|s| now.signed_duration_since(s.updated_at).num_milliseconds() <= SESSION_STALE_MS)
        .map_or(0, |s| s.running_subagents)
}

/// Background jobs (tests, builds, CI watches) the chat's runtime runs right
/// now, under the same staleness gate as [`running_subagents`]. Pure.
pub fn background_jobs(session: Option<&Session>, now: DateTime<Utc>) -> u32 {
    session
        .filter(|s| now.signed_duration_since(s.updated_at).num_milliseconds() <= SESSION_STALE_MS)
        .map_or(0, |s| s.background_jobs)
}

/// A chat's place in the engine's start queue (1-based), or 0 when it is not
/// waiting on capacity, under the same staleness gate as [`running_subagents`].
/// Only a `Working` row can be queued. Pure.
pub fn queue_position(session: Option<&Session>, now: DateTime<Utc>) -> u32 {
    session
        .filter(|s| now.signed_duration_since(s.updated_at).num_milliseconds() <= SESSION_STALE_MS)
        .filter(|s| s.status == SessionStatus::Working)
        .map_or(0, |s| s.queue_position)
}

/// The harness retry in flight for a chat: a fresh row (same gate as
/// [`effective_indicator`]) that is `Working` and carries a retry. Pure.
pub fn retrying(session: Option<&Session>, now: DateTime<Utc>) -> Option<&RetryState> {
    session
        .filter(|s| {
            s.status == SessionStatus::Working
                && now.signed_duration_since(s.updated_at).num_milliseconds() <= SESSION_STALE_MS
        })
        .and_then(|s| s.retry.as_ref())
}

/// One wording for a retry, shared by the sidebar and the MCP tools:
/// `Retrying · 2/10 · next try in 8s`. The countdown is dropped once
/// `next_attempt_at` has passed (or is unknown), `/max` when unknown.
pub fn retry_label(retry: &RetryState, now: DateTime<Utc>) -> String {
    let mut label = match retry.max_attempts {
        Some(max) => format!("Retrying · {}/{max}", retry.attempt),
        None => format!("Retrying · {}", retry.attempt),
    };
    if let Some(at) = retry.next_attempt_at {
        let ms = at.signed_duration_since(now).num_milliseconds();
        if ms > 0 {
            label.push_str(&format!(" · next try in {}s", (ms + 999) / 1000));
        }
    }
    label
}

/// The subagents running under a chat, under the same staleness gate as
/// [`running_subagents`]. Pure.
pub fn running_subagent_list(session: Option<&Session>, now: DateTime<Utc>) -> &[RunningSubagent] {
    session
        .filter(|s| now.signed_duration_since(s.updated_at).num_milliseconds() <= SESSION_STALE_MS)
        .map_or(&[], |s| s.subagents.as_slice())
}

/// The full display status for a chat row / tab dot: live states win, then the
/// synced seen marker decides completed-vs-idle. Staleness gating rides on
/// [`effective_indicator`]; the derivation itself is [`crate::chat_indicator`].
pub fn display_status(chat: &Chat, session: Option<&Session>, now: DateTime<Utc>) -> ChatIndicator {
    let live = session.filter(|s| effective_indicator(Some(s), now) != Indicator::None);
    crate::chat_indicator(chat, live)
}

/// Attention bucket for the sidebar's Active list — lower is more urgent.
pub fn attention_rank(status: ChatIndicator) -> u8 {
    match status {
        ChatIndicator::AwaitingInput => 0,
        ChatIndicator::Errored => 1,
        ChatIndicator::Working => 2,
        ChatIndicator::Completed => 3,
        ChatIndicator::Idle => 4,
    }
}

/// Which of the sidebar's activity groups a chat belongs to. Settled
/// statuses (Completed, Errored, Idle) belong to none: their row marks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Activity {
    NeedsInput,
    Working,
}

/// A chat's activity group: asking beats running, and a settled chat whose
/// subagents still run is working.
pub fn activity(status: ChatIndicator, running_subagents: u32) -> Option<Activity> {
    match status {
        ChatIndicator::AwaitingInput => Some(Activity::NeedsInput),
        ChatIndicator::Working => Some(Activity::Working),
        _ if running_subagents > 0 => Some(Activity::Working),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Sort orders
// ---------------------------------------------------------------------------

/// Active-list order: pure recency (`last_message_at` desc, `created_at`
/// fallback), id tiebreak so the sort is total. Deliberately NOT
/// attention-bucketed: status drives the DOT, never the position — bucketing
/// meant that merely OPENING a completed session (completed → seen → idle)
/// dropped its row under the pointer (user report: "their position in the
/// scrollbar changes"). Matches the old sidebar, which rendered chats in
/// recency order and let the dots carry urgency; [`attention_rank`] still
/// aggregates the space rows' urgency dot.
pub fn sort_active(rows: &mut Vec<(ChatIndicator, &Chat)>) {
    rows.sort_by(|(_, a), (_, b)| {
        let ka = a.last_message_at.unwrap_or(a.created_at);
        let kb = b.last_message_at.unwrap_or(b.created_at);
        kb.cmp(&ka).then_with(|| a.id.cmp(&b.id))
    });
}

/// Session-tab order for a space: creation order (activity never reorders
/// tabs), id tiebreak. Pure.
pub fn sort_tabs(chats: &mut [&Chat]) {
    chats.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// Spaces list order: creation order, id tiebreak — total and stable across
/// devices. Pure.
pub fn sort_spaces(spaces: &mut [Space]) {
    spaces.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// The project a space belongs to: spaces sharing a repository identity —
/// clones and worktrees of one repository, on any device — are one project.
/// A space without an identity is a project of its own. Pure.
pub fn project_key(space: &Space) -> String {
    match space.repository_id.as_deref() {
        Some(repository_id) => format!("repo:{repository_id}"),
        None => space.id.clone(),
    }
}

/// The space that speaks for `space`'s whole project: the oldest member, id
/// tiebreak. Its name and color stand for every member, and the choice is
/// the same on every device, so a repository looks alike everywhere. Pure.
pub fn representative_space<'a>(spaces: &'a [Space], space: &'a Space) -> &'a Space {
    let Some(repository_id) = space.repository_id.as_deref() else {
        return space;
    };
    spaces
        .iter()
        .filter(|s| s.repository_id.as_deref() == Some(repository_id))
        .min_by_key(|s| (s.created_at, s.id.as_str()))
        .unwrap_or(space)
}

/// Sidebar order: `last_message_at` desc, falling back to `created_at`; ties
/// break by `created_at` desc then id so the sort is total and stable across
/// devices. Pure.
pub fn sort_chats(chats: &mut [Chat]) {
    chats.sort_by(|a, b| {
        let ka = a.last_message_at.unwrap_or(a.created_at);
        let kb = b.last_message_at.unwrap_or(b.created_at);
        kb.cmp(&ka)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
}

// ---------------------------------------------------------------------------
// Boot gate
// ---------------------------------------------------------------------------

/// The app gate (zeron's App.tsx phases). Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatePhase {
    /// Booting / probing — splash covers this.
    Loading,
    /// Engine unreachable and embedding failed.
    Failed(String),
    /// Engine up, but signed out — show the sign-in card.
    SignIn,
    /// Signed in but no organization selected — "Create your workspace".
    OrgGate,
    /// Render the shell.
    Ready,
}

/// Missing scope is treated as synced. Current engines always publish
/// [`WorkspaceScope`] before becoming ready, while old daemons are deliberately
/// kept behind the account gate instead of being mistaken for local runtimes.
pub fn gate_phase(
    connection: &ConnectionStatus,
    workspace_scope: Option<WorkspaceScope>,
    auth: Option<&AuthState>,
) -> GatePhase {
    match connection {
        ConnectionStatus::Connecting => GatePhase::Loading,
        ConnectionStatus::Failed(err) => GatePhase::Failed(err.clone()),
        ConnectionStatus::Ready => match workspace_scope.unwrap_or(WorkspaceScope::Synced) {
            WorkspaceScope::Local | WorkspaceScope::Development => GatePhase::Ready,
            WorkspaceScope::Synced => match auth {
                Some(AuthState::NeedsOrganization { .. }) => GatePhase::OrgGate,
                Some(AuthState::SignedIn { .. }) => GatePhase::Ready,
                Some(AuthState::SignedOut) | None => GatePhase::SignIn,
            },
        },
    }
}

#[cfg(test)]
mod project_tests {
    use super::*;

    fn space(id: &str, device: &str, repository: Option<&str>, minutes: i64) -> Space {
        Space {
            id: id.into(),
            device_id: device.into(),
            path: format!("/{device}/{id}"),
            name: None,
            git_detected: repository.is_some(),
            git_checked_at: None,
            checkout_id: None,
            repository_id: repository.map(str::to_string),
            created_at: DateTime::<Utc>::UNIX_EPOCH + chrono::TimeDelta::minutes(minutes),
        }
    }

    #[test]
    fn spaces_sharing_a_repository_are_one_project_led_by_the_oldest() {
        let spaces = vec![
            space("laptop", "mac", Some("github.com/o/r"), 2),
            space("server", "vps", Some("github.com/o/r"), 1),
            space("notes", "mac", None, 0),
        ];
        assert_eq!(project_key(&spaces[0]), "repo:github.com/o/r");
        assert_eq!(project_key(&spaces[0]), project_key(&spaces[1]));
        assert_eq!(project_key(&spaces[2]), "notes");
        assert_eq!(representative_space(&spaces, &spaces[0]).id, "server");
        assert_eq!(representative_space(&spaces, &spaces[1]).id, "server");
        assert_eq!(representative_space(&spaces, &spaces[2]).id, "notes");
    }

    #[test]
    fn activity_groups_asking_and_running_chats() {
        use ChatIndicator::*;
        assert_eq!(activity(AwaitingInput, 0), Some(Activity::NeedsInput));
        assert_eq!(activity(AwaitingInput, 2), Some(Activity::NeedsInput));
        assert_eq!(activity(Working, 0), Some(Activity::Working));
        assert_eq!(activity(Idle, 2), Some(Activity::Working));
        assert_eq!(activity(Errored, 1), Some(Activity::Working));
        for settled in [Completed, Errored, Idle] {
            assert_eq!(activity(settled, 0), None);
        }
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use crate::UserProfile;

    fn user() -> UserProfile {
        UserProfile {
            id: "user-1".into(),
            email: "user@example.com".into(),
            name: None,
        }
    }

    fn running_session(running: u32, updated_at: DateTime<Utc>) -> Session {
        Session {
            last_completed_turn: None,
            chat_id: "chat".into(),
            device_id: "dev".into(),
            status: SessionStatus::Working,
            started_at: None,
            updated_at,
            retry: None,
            running_subagents: running,
            queue_position: 0,
            background_jobs: 0,
            subagents: (0..running)
                .map(|ix| RunningSubagent {
                    doc_id: format!("sub-{ix}"),
                    title: format!("Agent {ix}"),
                    started_at: updated_at,
                    fleet_row: None,
                })
                .collect(),
        }
    }

    #[test]
    fn running_subagents_count_only_while_the_session_row_is_fresh() {
        let now = Utc::now();
        let fresh = running_session(3, now - chrono::Duration::seconds(10));
        assert_eq!(running_subagents(Some(&fresh), now), 3);
        // A crashed engine's row must not badge a chat forever.
        let stale = running_session(
            3,
            now - chrono::Duration::milliseconds(SESSION_STALE_MS + 1),
        );
        assert_eq!(running_subagents(Some(&stale), now), 0);
        assert_eq!(running_subagents(None, now), 0);
        assert_eq!(running_subagent_list(Some(&fresh), now).len(), 3);
        assert!(running_subagent_list(Some(&stale), now).is_empty());
        assert!(running_subagent_list(None, now).is_empty());
    }

    fn retry_state(attempt: u32, max: Option<u32>, next: Option<DateTime<Utc>>) -> RetryState {
        RetryState {
            attempt,
            max_attempts: max,
            next_attempt_at: next,
            error: None,
        }
    }

    #[test]
    fn retrying_needs_a_fresh_working_row_with_a_retry() {
        let now = Utc::now();
        let mut row = running_session(0, now - chrono::Duration::seconds(5));
        assert!(retrying(Some(&row), now).is_none());
        row.retry = Some(retry_state(2, Some(10), None));
        assert_eq!(retrying(Some(&row), now).map(|r| r.attempt), Some(2));
        // A crashed engine's row never shows an eternal retry.
        row.updated_at = now - chrono::Duration::milliseconds(SESSION_STALE_MS + 1);
        assert!(retrying(Some(&row), now).is_none());
        row.updated_at = now;
        row.status = SessionStatus::Idle;
        assert!(retrying(Some(&row), now).is_none());
        assert!(retrying(None, now).is_none());
    }

    #[test]
    fn retry_label_counts_down_and_drops_what_it_does_not_know() {
        let now = Utc::now();
        let soon = now + chrono::Duration::milliseconds(7_200);
        assert_eq!(
            retry_label(&retry_state(2, Some(10), Some(soon)), now),
            "Retrying · 2/10 · next try in 8s"
        );
        assert_eq!(
            retry_label(&retry_state(2, None, None), now),
            "Retrying · 2"
        );
        let past = now - chrono::Duration::seconds(3);
        assert_eq!(
            retry_label(&retry_state(1, Some(3), Some(past)), now),
            "Retrying · 1/3"
        );
    }

    #[test]
    fn queue_position_counts_only_a_fresh_working_row() {
        let now = Utc::now();
        let mut queued = running_session(0, now - chrono::Duration::seconds(10));
        queued.queue_position = 2;
        assert_eq!(queue_position(Some(&queued), now), 2);
        let mut stale = queued.clone();
        stale.updated_at = now - chrono::Duration::milliseconds(SESSION_STALE_MS + 1);
        assert_eq!(queue_position(Some(&stale), now), 0);
        let mut settled = queued.clone();
        settled.status = SessionStatus::Idle;
        assert_eq!(queue_position(Some(&settled), now), 0);
        assert_eq!(queue_position(None, now), 0);
    }

    #[test]
    fn workspace_scope_controls_the_auth_gate() {
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Local),
                Some(&AuthState::SignedOut),
            ),
            GatePhase::Ready
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::SignedOut),
            ),
            GatePhase::SignIn
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::NeedsOrganization { user: user() }),
            ),
            GatePhase::OrgGate
        );
    }

    #[test]
    fn development_and_local_never_use_the_workos_gate() {
        for scope in [WorkspaceScope::Local, WorkspaceScope::Development] {
            for auth in [
                AuthState::SignedOut,
                AuthState::NeedsOrganization { user: user() },
                AuthState::SignedIn {
                    user: user(),
                    org_id: Some("org-1".into()),
                },
            ] {
                assert_eq!(
                    gate_phase(&ConnectionStatus::Ready, Some(scope), Some(&auth)),
                    GatePhase::Ready
                );
            }
        }
    }

    #[test]
    fn missing_scope_falls_back_to_a_synced_gate() {
        assert_eq!(
            gate_phase(&ConnectionStatus::Ready, None, None),
            GatePhase::SignIn
        );
    }
}

/// Parse an `AuthStatus` frame tolerantly. The engine currently serializes its
/// own enum (`{"_tag": "SignedIn", ...}`) while the proto type expects
/// `{"state": "signedIn", ...}` — accept both so either side can converge
/// without breaking a viewport.
pub fn parse_auth_state(value: &serde_json::Value) -> Option<AuthState> {
    if let Ok(state) = serde_json::from_value::<AuthState>(value.clone()) {
        return Some(state);
    }
    let tag = value.get("_tag").and_then(|t| t.as_str())?;
    let user = || -> Option<crate::UserProfile> {
        let u = value.get("user")?;
        Some(crate::UserProfile {
            id: u.get("id")?.as_str()?.to_string(),
            email: u.get("email")?.as_str()?.to_string(),
            name: u.get("name").and_then(|n| n.as_str()).map(str::to_string),
        })
    };
    match tag {
        "SignedOut" => Some(AuthState::SignedOut),
        "NeedsOrganization" => Some(AuthState::NeedsOrganization { user: user()? }),
        "SignedIn" => Some(AuthState::SignedIn {
            user: user()?,
            org_id: value
                .get("orgId")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Sidebar grouping
// ---------------------------------------------------------------------------

/// One grouped-by-project sidebar section.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatGroup<'a> {
    pub label: String,
    pub chats: Vec<&'a Chat>,
}

/// Project label for a chat: the basename of its cwd, or "No project".
pub fn project_label(cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.map(str::trim).filter(|c| !c.is_empty()) else {
        return "No project".to_string();
    };
    if crate::projectless::is_projectless_cwd(Some(cwd)) {
        return "No project".to_string();
    }
    std::path::Path::new(cwd.trim_end_matches(['/', '\\']))
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| cwd.to_string())
}

/// Group chats by project label, preserving the incoming (recency) order both
/// for groups (by their most recent chat) and rows within a group. Pure.
pub fn group_chats<'a>(chats: impl IntoIterator<Item = &'a Chat>) -> Vec<ChatGroup<'a>> {
    let mut groups: Vec<ChatGroup<'a>> = Vec::new();
    for chat in chats {
        let label = project_label(chat.cwd.as_deref());
        match groups.iter_mut().find(|g| g.label == label) {
            Some(group) => group.chats.push(chat),
            None => groups.push(ChatGroup {
                label,
                chats: vec![chat],
            }),
        }
    }
    groups
}

/// Compact relative time ("now", "5m", "3h", "2d", "1w", …) — no "ago" suffix;
/// port of zeron's `formatTimeAgo`.
pub fn format_time_ago(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let s = now.signed_duration_since(then).num_seconds().max(0);
    // Under a minute reads as "now" — otherwise 45–59s floors to a bare "0m".
    if s < 60 {
        return "now".to_string();
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h");
    }
    let d = h / 24;
    if d < 7 {
        return format!("{d}d");
    }
    let w = d / 7;
    if w < 5 {
        return format!("{w}w");
    }
    let mo = d / 30;
    if mo < 12 {
        return format!("{mo}mo");
    }
    format!("{}y", d / 365)
}

/// Session-row sub-line, "project · branch" (zeron `chatLocation`): the repo
/// checkout identity. Either part may be missing; empty when both are. A
/// projectless chat names no project and no folder.
pub fn chat_location(chat: &Chat) -> Option<String> {
    let project = chat
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|c| !crate::projectless::is_projectless_cwd(Some(c)))
        .map(|c| project_label(Some(c)));
    let reference = chat
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty());
    match (project, reference) {
        (Some(p), Some(r)) => Some(format!("{p} · {r}")),
        (Some(p), None) => Some(p),
        (None, Some(r)) => Some(r.to_string()),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------
// Tool summaries (pure)
// ---------------------------------------------------------------------------

/// Collapse model-generated text onto ONE line for single-line surfaces (tool
/// chips, titles, previews): newlines, tabs and runs of whitespace become
/// single spaces, trimmed.
///
/// Both viewports need this for the same reason from opposite directions — gpui
/// breaks on a literal `\n` before its ellipsis logic, and a terminal cell grid
/// would take an embedded newline as a cursor move.
pub fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

/// Per-kind chip label + one-line detail. Labels match zeron's `describeTool`
/// (tool-chip.tsx) exactly, so the two viewports name a tool identically.
pub fn tool_chip_content(call: &crate::ToolCall) -> (&'static str, String) {
    let (label, detail) = tool_chip_content_raw(call);
    (label, single_line(&detail))
}

fn tool_chip_content_raw(call: &crate::ToolCall) -> (&'static str, String) {
    use crate::ToolCall;
    match call {
        ToolCall::Exec { command } => ("Run", command.clone()),
        ToolCall::ReadFile { path } => ("Read", path.clone()),
        ToolCall::WriteFile { path, .. } => ("Write", path.clone()),
        ToolCall::EditFile { path, .. } => ("Edit", path.clone()),
        ToolCall::ApplyPatch { path } => {
            ("Patch", path.clone().unwrap_or_else(|| "workspace".into()))
        }
        ToolCall::Search { pattern, path } => (
            "Search",
            match path {
                Some(path) => format!("{pattern} in {path}"),
                None => pattern.clone(),
            },
        ),
        ToolCall::Glob { pattern } => ("Glob", pattern.clone()),
        ToolCall::WebFetch { url, .. } => ("Fetch", url.clone()),
        ToolCall::WebSearch { query } => ("Web", query.clone()),
        ToolCall::Todo { items } => {
            let done = items.iter().filter(|i| i.done).count();
            ("Todo", format!("{done}/{} done", items.len()))
        }
        ToolCall::Mcp {
            server,
            tool,
            input,
        } if server == PI_EXTENSION_SERVER => pi_extension_chip(tool, input.as_ref()),
        ToolCall::Mcp { server, tool, .. } => ("MCP", format!("{server} · {tool}")),
        // Subagent spawns decode as Unknown named "Agent[: <description>]"
        // (every native driver's convention): label them "Agent" with the
        // description as the detail — "Tool · Agent: scan repo" read as two
        // labels fighting.
        ToolCall::Unknown { name, input } => match name.strip_prefix("Agent: ") {
            Some(description) => ("Agent", description.to_owned()),
            None if name == "Agent" => ("Agent", String::new()),
            None => unknown_chip(name, input.as_ref()),
        },
    }
}

/// A tool no driver decodes, named for what it is where the name says so:
/// Pi's `ask_user`, the LSP tools, an extension's own tools (`workflow`,
/// `memory_search`, `goal_plan`).
fn unknown_chip(name: &str, input: Option<&serde_json::Value>) -> (&'static str, String) {
    if let Some(target) = name.strip_prefix("LSP: ") {
        return ("LSP", target.to_owned());
    }
    if name == "ask_user" {
        let question = input
            .and_then(|input| input.get("question"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let count = input
            .and_then(|input| input.get("questions"))
            .and_then(serde_json::Value::as_array)
            .map(Vec::len);
        let detail = match (question, count) {
            (Some(question), _) => question,
            (None, Some(1)) => "1 question".into(),
            (None, Some(n)) => format!("{n} questions"),
            (None, None) => String::new(),
        };
        return ("Question", detail);
    }
    match pi_extension_label(name) {
        "Tool" | "Note" => ("Tool", name.to_owned()),
        label => (label, name.to_owned()),
    }
}

/// The `server` of the tool rows Pi's extensions put in the transcript: the
/// agent's own work they draw (the advisor, Jev, subagent fleets, background
/// jobs, notices) and the messages they display. `tool` names the extension
/// (`advisor-live-panel`) or the message type (`subagent-result`).
pub const PI_EXTENSION_SERVER: &str = "pi";

/// A plain widget that only restates the running goal (minimal-status
/// repaints `goal  36.7k tok · steps 0/3 · 20s` every second). The
/// transcript's goal line and the footer chip show the goal natively, so it
/// is never recorded as a row, and rows recorded before are not drawn.
pub fn is_goal_widget(key: &str) -> bool {
    key == "goal" || key.ends_with(":goal")
}

/// The label a Pi extension's row wears, named like the stock tool it is
/// closest to ("Run" for background shells, "Agent" for subagents).
pub fn pi_extension_label(tool: &str) -> &'static str {
    // `pi-advisor-review`, `pi-herdr-peer`: the extension, not the prefix.
    let named = tool.strip_prefix("pi-").unwrap_or(tool);
    let family = named.split([':', '-', '_', '.']).next().unwrap_or(named);
    match family {
        "advisor" => "Advisor",
        "jev" => "Jev",
        "decisions" => "Decisions",
        "subagent" | "subagents" | "delegate" => "Agent",
        "bash" => "Run",
        "workflow" | "workflows" => "Workflow",
        "todo" => "Todo",
        "web" => "Web",
        "memory" => "Memory",
        "refusal" | "claude" => "Claude",
        "herdr" => "Agent",
        "pi" if tool.contains("herdr") => "Agent",
        "rtk" => "Run",
        "goal" => "Goal",
        "notice" => "Note",
        _ => "Tool",
    }
}

fn pi_extension_chip(tool: &str, input: Option<&serde_json::Value>) -> (&'static str, String) {
    let text = |key: &str| {
        input
            .and_then(|input| input.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let detail = text("title")
        .or_else(|| text("message"))
        .or_else(|| text("description"))
        .or_else(|| text("label"))
        .unwrap_or_else(|| tool.to_owned());
    (pi_extension_label(tool), detail)
}

/// The ToolGroup summary line — "Ran 3 commands · edited 2 files".
///
/// Takes `(call, is_error)` pairs so each viewport can keep its own row model;
/// the summary itself is one implementation for both.
pub fn tool_group_summary(tools: &[(crate::ToolCall, bool)]) -> String {
    use crate::ToolCall;
    let mut commands = 0usize;
    let mut edited: Vec<&str> = Vec::new();
    let mut reads = 0usize;
    let mut searches = 0usize;
    let mut fetches = 0usize;
    let mut todos = 0usize;
    let mut other = 0usize;
    let mut failed = 0usize;
    for (call, is_error) in tools {
        if *is_error {
            failed += 1;
        }
        match call {
            ToolCall::Exec { .. } => commands += 1,
            ToolCall::WriteFile { path, .. } | ToolCall::EditFile { path, .. } => {
                if !edited.contains(&path.as_str()) {
                    edited.push(path);
                }
            }
            ToolCall::ApplyPatch { path } => {
                let p = path.as_deref().unwrap_or("patch");
                if !edited.contains(&p) {
                    edited.push(p);
                }
            }
            ToolCall::ReadFile { .. } => reads += 1,
            ToolCall::Search { .. } | ToolCall::Glob { .. } | ToolCall::WebSearch { .. } => {
                searches += 1
            }
            ToolCall::WebFetch { .. } => fetches += 1,
            ToolCall::Todo { .. } => todos += 1,
            ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => other += 1,
        }
    }
    let mut segments: Vec<String> = Vec::new();
    if commands > 0 {
        segments.push(format!("ran {}", plural(commands, "command", "commands")));
    }
    if !edited.is_empty() {
        segments.push(format!("edited {}", plural(edited.len(), "file", "files")));
    }
    if reads > 0 {
        segments.push(format!("read {}", plural(reads, "file", "files")));
    }
    if searches > 0 {
        segments.push(format!("searched {}", plural(searches, "time", "times")));
    }
    if fetches > 0 {
        segments.push(format!("fetched {}", plural(fetches, "page", "pages")));
    }
    if todos > 0 {
        segments.push("updated todos".to_string());
    }
    if other > 0 {
        segments.push(format!("called {}", plural(other, "tool", "tools")));
    }
    if segments.is_empty() {
        segments.push(plural(tools.len(), "tool", "tools"));
    }
    if failed > 0 {
        segments.push(format!("{failed} failed"));
    }
    let mut summary = segments.join(" · ");
    // Capitalize the first segment only (zeron's style).
    if let Some(first) = summary.get(0..1) {
        let upper = first.to_uppercase();
        summary.replace_range(0..1, &upper);
    }
    summary
}

/// The status-dot palette, as oklch triples (L, C, H°).
///
/// Colors live here rather than in the viewport because the *meaning* of a
/// dot is part of the protocol, not the presentation — a given status must
/// read the same on every surface. `zeron-ui` has the oklch→sRGB math.
pub mod dot {
    /// Running. Pink, not amber: the harsh yellow read as a warning, and running
    /// is routine (user request).
    pub const WORKING: (f32, f32, f32) = (0.718, 0.202, 349.761);
    /// Asking a question. Indigo — must read differently from "busy" at a glance.
    pub const AWAITING: (f32, f32, f32) = (0.673, 0.182, 276.935);
    /// Errored. Red-400.
    pub const ERRORED: (f32, f32, f32) = (0.704, 0.191, 22.216);
    /// Finished but unseen. Emerald — reads as "ready for you".
    pub const COMPLETED: (f32, f32, f32) = (0.765, 0.177, 163.223);
}

// ---------------------------------------------------------------------------
// Checkout selection (new sessions)
// ---------------------------------------------------------------------------

/// Where a new session runs (t3code's env-mode: `local | worktree`).
///
/// "Current worktree" is deliberately **not** a third mode — it is `Local` when
/// the picked ref already happens to be materialized as a worktree, in which
/// case the session reuses that checkout's path. Modelling it as three states
/// would let the UI hold a combination the engine cannot honour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckoutKind {
    /// The space's own folder — or the picked ref's existing worktree.
    #[default]
    Local,
    /// A fresh isolated worktree created off the picked base ref on send.
    NewWorktree,
}

/// The resolved on-send checkout action.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckoutPlan {
    /// Run in the space folder as-is. `branch` is the checkout's branch (the
    /// picked or current ref), carried onto `createChat` so the session names
    /// it from the first frame; `None` = refs never loaded.
    CurrentCheckout { branch: Option<String> },
    /// Reuse the picked ref's existing worktree (a cwd override; no git).
    ReuseWorktree { path: String, branch: String },
    /// `CreateWorktree` off `base` on send (the engine mints a `zeron/<name>`
    /// branch). `base: None` = refs never loaded — send falls back to the space
    /// folder rather than failing.
    NewWorktree { base: Option<String> },
}

/// Resolve the on-send action from the mode and the picked ref.
pub fn checkout_plan(kind: CheckoutKind, picked: Option<&crate::RepoRef>) -> CheckoutPlan {
    let name = picked.map(|r| r.name.clone());
    match kind {
        CheckoutKind::NewWorktree => CheckoutPlan::NewWorktree { base: name },
        CheckoutKind::Local => match picked.and_then(|r| r.worktree_path.clone()) {
            Some(path) => CheckoutPlan::ReuseWorktree {
                path,
                branch: name.unwrap_or_default(),
            },
            None => CheckoutPlan::CurrentCheckout { branch: name },
        },
    }
}

/// Label of the checkout-kind trigger (t3code `resolveEnvModeLabel`).
pub fn checkout_label(kind: CheckoutKind, picked: Option<&crate::RepoRef>) -> &'static str {
    match kind {
        CheckoutKind::NewWorktree => "New worktree",
        CheckoutKind::Local => {
            if picked.is_some_and(|r| r.worktree_path.is_some()) {
                "Current worktree"
            } else {
                "Current checkout"
            }
        }
    }
}

#[cfg(test)]
mod checkout_tests {
    use super::*;
    use crate::RepoRef;

    fn plain(name: &str) -> RepoRef {
        RepoRef {
            name: name.into(),
            current: false,
            worktree_path: None,
        }
    }

    fn materialized(name: &str, path: &str) -> RepoRef {
        RepoRef {
            name: name.into(),
            current: false,
            worktree_path: Some(path.into()),
        }
    }

    #[test]
    fn local_resolves_by_whether_the_ref_has_a_worktree() {
        // The same mode means two different things depending on the ref — which
        // is exactly why "current worktree" is not its own state.
        assert_eq!(
            checkout_plan(CheckoutKind::Local, Some(&plain("main"))),
            CheckoutPlan::CurrentCheckout {
                branch: Some("main".into())
            }
        );
        assert_eq!(
            checkout_plan(CheckoutKind::Local, Some(&materialized("feat", "/wt/feat"))),
            CheckoutPlan::ReuseWorktree {
                path: "/wt/feat".into(),
                branch: "feat".into()
            }
        );
        // No ref picked at all is still the space folder — with no branch to
        // stamp until refs load.
        assert_eq!(
            checkout_plan(CheckoutKind::Local, None),
            CheckoutPlan::CurrentCheckout { branch: None }
        );
    }

    #[test]
    fn new_worktree_carries_its_base_and_tolerates_none() {
        assert_eq!(
            checkout_plan(CheckoutKind::NewWorktree, Some(&plain("main"))),
            CheckoutPlan::NewWorktree {
                base: Some("main".into())
            }
        );
        // Refs never loaded: send falls back to the space folder rather than
        // failing, so the base is allowed to be absent.
        assert_eq!(
            checkout_plan(CheckoutKind::NewWorktree, None),
            CheckoutPlan::NewWorktree { base: None }
        );
    }

    #[test]
    fn labels_say_which_of_the_three_outcomes_you_will_get() {
        assert_eq!(
            checkout_label(CheckoutKind::Local, Some(&plain("main"))),
            "Current checkout"
        );
        assert_eq!(
            checkout_label(CheckoutKind::Local, Some(&materialized("f", "/wt/f"))),
            "Current worktree"
        );
        assert_eq!(
            checkout_label(CheckoutKind::NewWorktree, Some(&plain("main"))),
            "New worktree"
        );
    }
}

#[cfg(test)]
mod pi_extension_tests {
    use super::*;
    use crate::ToolCall;

    fn chip(tool: &str, input: serde_json::Value) -> (&'static str, String) {
        tool_chip_content(&ToolCall::Mcp {
            server: PI_EXTENSION_SERVER.into(),
            tool: tool.into(),
            input: Some(input),
        })
    }

    #[test]
    fn pi_extension_rows_are_named_like_the_stock_tool_they_resemble() {
        assert_eq!(
            chip("advisor-live-panel", serde_json::json!({"title":"Advisor · review"})),
            ("Advisor", "Advisor · review".to_string())
        );
        assert_eq!(chip("jev", serde_json::json!({})), ("Jev", "jev".to_string()));
        assert_eq!(
            chip("bash", serde_json::json!({"title":"Background bash · 1 running"})).0,
            "Run"
        );
        assert_eq!(chip("subagent-result", serde_json::json!({})).0, "Agent");
        assert_eq!(chip("pi-herdr-delegate.monitor", serde_json::json!({})).0, "Agent");
        assert_eq!(chip("workflow-result", serde_json::json!({})).0, "Workflow");
        assert_eq!(
            chip("notice", serde_json::json!({"message":"Saved\n the plan"})),
            ("Note", "Saved the plan".to_string())
        );
        assert_eq!(chip("someone-else", serde_json::json!({})).0, "Tool");
        assert_eq!(chip("pi-advisor-review", serde_json::json!({})).0, "Advisor");
        assert_eq!(chip("pi-herdr-peer", serde_json::json!({})).0, "Agent");
        assert_eq!(
            tool_chip_content(&ToolCall::Unknown {
                name: "ask_user".into(),
                input: Some(serde_json::json!({"question":"Which port?"})),
            }),
            ("Question", "Which port?".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::Unknown {
                name: "LSP: diagnostics src/main.rs".into(),
                input: None,
            }),
            ("LSP", "diagnostics src/main.rs".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::Unknown {
                name: "workflow".into(),
                input: None,
            })
            .0,
            "Workflow"
        );
        // Other MCP servers keep their label.
        assert_eq!(
            tool_chip_content(&ToolCall::Mcp {
                server: "gh".into(),
                tool: "issues".into(),
                input: None
            }),
            ("MCP", "gh · issues".to_string())
        );
    }
}
