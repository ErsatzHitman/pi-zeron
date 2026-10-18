//! Stop an orchestrator's whole tree, or one branch of it, for good.
//!
//! A Stop ends a turn; a Pi runtime outlives it, with its background shells
//! and sub-agents, its queued messages and every way back into work: the
//! orchestrator's next message, a child's outcome note, a boot-time revival.
//! Stopping a runaway tree needs all of those closed at once, so this
//! tears every runtime down ([`DocHost::stop_chat_work`]), empties the
//! queues, holds the tree (the project's pause flag, which the orchestrator's
//! own tools, outcome notes and boot recovery all honor) and then reports
//! what is still running instead of claiming success.
//!
//! Only sessions this device hosts are stopped here. The ones on other
//! devices come back in the report for the caller to stop there (the call is
//! device-addressable, and `only` limits it to the ones named).

use std::collections::HashSet;
use std::time::Duration;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use zeron_proto::orchestrator::{is_root_chat, lineage, subtree_members, tree_members};
use zeron_proto::{Chat, SessionStatus};

use crate::doc_host::DocHost;
use crate::sessions::SessionsEngine;
use crate::workspace_host::WorkspaceHost;

/// How long to wait, in all, for the stopped runtimes to be gone.
const SETTLE: Duration = Duration::from_secs(15);
/// Sessions torn down at once.
const PARALLEL: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopScope {
    /// `chatId` is an orchestrator root: every session of its tree.
    #[default]
    Tree,
    /// `chatId` and everything below it.
    Subtree,
}

#[derive(Debug, Clone)]
pub struct StopTreeRequest {
    pub chat_id: String,
    pub scope: StopScope,
    /// Hold the tree so nothing restarts by itself (whole tree only).
    pub hold: bool,
    /// Stop only these (a call forwarded to the device that hosts them).
    pub only: Option<Vec<String>>,
    /// The session asking: it is never stopped, and may only stop what is
    /// below it.
    pub caller: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopTreeError {
    /// The request names something it may not stop.
    BadParams(String),
    Failed(String),
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StopTreeReport {
    /// Sessions whose runtime or queue this call ended.
    pub stopped: Vec<StoppedChat>,
    /// Sessions that had nothing running or waiting.
    pub already_idle: Vec<String>,
    /// Queued and held messages removed.
    pub queued_cleared: usize,
    /// The tree is held: nothing in it restarts until it is resumed.
    pub held: bool,
    /// Still running when the wait ended.
    pub remaining: Vec<RemainingChat>,
    /// Hosted by another device: stop them there.
    pub remote: Vec<RemoteChat>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StoppedChat {
    pub chat_id: String,
    pub title: Option<String>,
    pub was_status: Option<SessionStatus>,
    pub background_jobs: u32,
    pub running_subagents: u32,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemainingChat {
    pub chat_id: String,
    pub reason: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteChat {
    pub chat_id: String,
    pub device_id: String,
}

/// The sessions `request` names, parents before their children, after the
/// checks on who may stop what. The caller's own chat never appears.
pub fn select_targets<'a>(
    chats: &'a [Chat],
    request: &StopTreeRequest,
) -> Result<Vec<&'a Chat>, StopTreeError> {
    let bad = |text: &str| StopTreeError::BadParams(text.to_owned());
    let named = chats
        .iter()
        .find(|chat| chat.id == request.chat_id)
        .ok_or_else(|| StopTreeError::BadParams(format!("unknown chat {}", request.chat_id)))?;
    let mut targets: Vec<&Chat> = match request.scope {
        StopScope::Tree => {
            if !is_root_chat(&named.id) {
                return Err(bad(
                    "Only an orchestrator's whole tree can be stopped at once.",
                ));
            }
            if request
                .caller
                .as_deref()
                .is_some_and(|caller| caller != named.id)
            {
                return Err(bad("Only the orchestrator can stop its whole tree."));
            }
            std::iter::once(named)
                .chain(tree_members(chats, &named.id))
                .collect()
        }
        StopScope::Subtree => {
            if let Some(caller) = request.caller.as_deref() {
                // The caller itself or one of its ancestors named: everything
                // below the caller. Anything else is not the caller's to stop.
                let above = lineage(chats, &named.id)
                    .iter()
                    .any(|chat| chat.id == caller);
                if !above {
                    return Err(bad("You can only stop sessions below you."));
                }
            }
            std::iter::once(named)
                .chain(subtree_members(chats, &named.id))
                .collect()
        }
    };
    if let Some(caller) = request.caller.as_deref() {
        targets.retain(|chat| chat.id != caller);
    }
    if let Some(only) = &request.only {
        targets.retain(|chat| only.contains(&chat.id));
    }
    Ok(targets)
}

/// Stop what `request` names on this device and say what happened.
pub async fn stop_tree(
    workspace: &WorkspaceHost,
    doc_host: &DocHost,
    sessions: &SessionsEngine,
    request: &StopTreeRequest,
) -> Result<StopTreeReport, StopTreeError> {
    let failed = |e: crate::EngineError| StopTreeError::Failed(e.to_string());
    let chats = workspace.read_chats().map_err(failed)?;
    let targets = select_targets(&chats, request)?;
    let mut report = StopTreeReport::default();

    // First, so nothing the tree's agents do while it comes down starts new
    // work: the orchestrator's tools refuse, outcome notes are dropped and a
    // boot recovery leaves the members down.
    if request.hold
        && request.scope == StopScope::Tree
        && let Some(root) = zeron_proto::orchestrator::root_of(&chats, &request.chat_id)
    {
        let project = workspace
            .update_orchestrator_project(
                &root.id,
                zeron_proto::orchestrator_project::ProjectUpdate {
                    paused: Some(true),
                    ..Default::default()
                },
            )
            .map_err(failed)?;
        report.held = project.paused;
    }

    let here = doc_host.device_id();
    let mut local: Vec<&Chat> = Vec::new();
    for chat in targets {
        if chat.device_id == here {
            local.push(chat);
        } else {
            report.remote.push(RemoteChat {
                chat_id: chat.id.clone(),
                device_id: chat.device_id.clone(),
            });
        }
    }

    // What each session held before it came down, for the report.
    let before: Vec<(String, Option<zeron_proto::Session>)> = local
        .iter()
        .map(|chat| (chat.id.clone(), sessions.session_status(&chat.id)))
        .collect();
    // Owned ids: an iterator over borrowed chats cannot be held across the
    // awaits below in a future the RPC server requires to be Send.
    let target_ids: Vec<String> = local.iter().map(|chat| chat.id.clone()).collect();
    let outcomes: Vec<(
        String,
        Result<crate::doc_host::ChatStop, crate::EngineError>,
    )> = futures::stream::iter(target_ids)
        .map(|id| async move {
            let outcome = doc_host.stop_chat_work(sessions, &id).await;
            (id, outcome)
        })
        .buffer_unordered(PARALLEL)
        .collect()
        .await;

    // Teardown is asynchronous in places (a stopped Pi takes its shells down
    // with it): wait for the runtimes to be gone, within bounds.
    let ids: Vec<&str> = local.iter().map(|chat| chat.id.as_str()).collect();
    let deadline = tokio::time::Instant::now() + SETTLE;
    while tokio::time::Instant::now() < deadline
        && ids.iter().any(|id| still_running(sessions, id).is_some())
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let titles = |id: &str| {
        chats
            .iter()
            .find(|chat| chat.id == id)
            .and_then(|chat| chat.title.clone())
    };
    let mut errors: HashSet<String> = HashSet::new();
    for (id, outcome) in outcomes {
        let row = before
            .iter()
            .find(|(before_id, _)| *before_id == id)
            .and_then(|(_, session)| session.as_ref());
        match outcome {
            Ok(stop) => {
                report.queued_cleared += stop.queued_cleared;
                if stop.ended || stop.queued_cleared > 0 {
                    report.stopped.push(StoppedChat {
                        title: titles(&id),
                        was_status: row.map(|session| session.status),
                        background_jobs: row.map_or(0, |session| session.background_jobs),
                        running_subagents: row.map_or(0, |session| session.running_subagents),
                        chat_id: id,
                    });
                } else {
                    report.already_idle.push(id);
                }
            }
            Err(error) => {
                errors.insert(id.clone());
                report.remaining.push(RemainingChat {
                    chat_id: id,
                    reason: format!("could not be stopped: {error}"),
                });
            }
        }
    }
    for id in ids {
        if errors.contains(id) {
            continue;
        }
        if let Some(reason) = still_running(sessions, id) {
            report.remaining.push(RemainingChat {
                chat_id: id.to_owned(),
                reason: reason.to_owned(),
            });
        }
    }
    Ok(report)
}

/// Why `chat_id` still counts as running, `None` once nothing of it is.
fn still_running(sessions: &SessionsEngine, chat_id: &str) -> Option<&'static str> {
    if sessions.has_run(chat_id) {
        Some("its runtime is still shutting down")
    } else if sessions.holds_background_work(chat_id) {
        Some("it still reports background work")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::orchestrator::new_root_chat_id;

    fn chat(id: &str, parent: Option<&str>) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": "dev", "title": null, "archived": false,
            "cwd": null, "branch": null, "checkoutId": null, "config": null,
            "lastMessagePreview": null, "lastMessageAt": null,
            "createdAt": "2026-10-14T00:00:00Z", "parentChatId": parent,
        }))
        .unwrap()
    }

    fn request(chat_id: &str, scope: StopScope) -> StopTreeRequest {
        StopTreeRequest {
            chat_id: chat_id.into(),
            scope,
            hold: true,
            only: None,
            caller: None,
        }
    }

    fn ids(chats: &[&Chat]) -> Vec<String> {
        chats.iter().map(|chat| chat.id.clone()).collect()
    }

    fn tree() -> (String, Vec<Chat>) {
        let root = new_root_chat_id();
        let chats = vec![
            chat(&root, None),
            chat("a", Some(&root)),
            chat("b", Some(&root)),
            chat("a1", Some("a")),
            chat("other-root-child", Some("elsewhere")),
        ];
        (root, chats)
    }

    #[test]
    fn a_whole_tree_stop_names_the_root_and_every_member() {
        let (root, chats) = tree();
        let targets = select_targets(&chats, &request(&root, StopScope::Tree)).unwrap();
        assert_eq!(ids(&targets), [root.as_str(), "a", "b", "a1"]);
    }

    #[test]
    fn the_orchestrator_stopping_its_tree_never_stops_itself() {
        let (root, chats) = tree();
        let mut ask = request(&root, StopScope::Tree);
        ask.caller = Some(root.clone());
        let targets = select_targets(&chats, &ask).unwrap();
        assert_eq!(ids(&targets), ["a", "b", "a1"]);
    }

    #[test]
    fn a_member_cannot_stop_the_whole_tree_or_a_sibling_or_its_parent() {
        let (root, chats) = tree();
        let mut ask = request(&root, StopScope::Tree);
        ask.caller = Some("a".into());
        assert!(matches!(
            select_targets(&chats, &ask),
            Err(StopTreeError::BadParams(_))
        ));
        for target in ["b", root.as_str()] {
            let mut ask = request(target, StopScope::Subtree);
            ask.caller = Some("a".into());
            assert!(
                matches!(
                    select_targets(&chats, &ask),
                    Err(StopTreeError::BadParams(_))
                ),
                "{target}"
            );
        }
        let mut below = request("a1", StopScope::Subtree);
        below.caller = Some("a".into());
        assert_eq!(ids(&select_targets(&chats, &below).unwrap()), ["a1"]);
        // Naming itself means everything below it, never itself.
        let mut own = request("a", StopScope::Subtree);
        own.caller = Some("a".into());
        assert_eq!(ids(&select_targets(&chats, &own).unwrap()), ["a1"]);
    }

    #[test]
    fn a_branch_stop_spares_the_rest_of_the_tree() {
        let (root, chats) = tree();
        let targets = select_targets(&chats, &request("a", StopScope::Subtree)).unwrap();
        assert_eq!(ids(&targets), ["a", "a1"]);
        let mut ask = request(&root, StopScope::Subtree);
        ask.caller = Some(root.clone());
        let all_below = select_targets(&chats, &ask).unwrap();
        assert_eq!(ids(&all_below), ["a", "b", "a1"]);
    }

    #[test]
    fn only_limits_a_forwarded_stop_to_the_sessions_named() {
        let (root, chats) = tree();
        let mut ask = request(&root, StopScope::Tree);
        ask.only = Some(vec!["b".into(), "a1".into()]);
        assert_eq!(ids(&select_targets(&chats, &ask).unwrap()), ["b", "a1"]);
    }

    #[test]
    fn unknown_chats_and_non_roots_are_refused() {
        let (_, chats) = tree();
        assert!(matches!(
            select_targets(&chats, &request("nope", StopScope::Tree)),
            Err(StopTreeError::BadParams(_))
        ));
        assert!(matches!(
            select_targets(&chats, &request("a", StopScope::Tree)),
            Err(StopTreeError::BadParams(_))
        ));
    }
}
