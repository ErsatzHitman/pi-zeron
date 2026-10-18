//! The tree policy as the engine enforces it and the Pi guard reads it.
//!
//! The user sets a [`TreePolicy`] on an orchestrator's project (registry,
//! synced). Pi's guard cannot read the registry, so the engine keeps one file
//! per orchestrator under `{data_dir}/tree-policy`, named in `ZERON_TREE_POLICY`
//! for every Pi of the tree: where tests run, how many heavy commands at once,
//! which models, and whether the spending cap halted the tree. The guard
//! re-reads the file when it changes, so an edit reaches running sessions.
//!
//! A tree's spend is counted from Pi's own files ([`tree_spend`]): the
//! requests the Pi sessions of the tree's chats made, on this device. The
//! [`TreeWatcher`] checks every orchestrator hosted here with a cap, tells it
//! to wrap up at the warning percentage, and at the cap pauses the tree. A
//! pause never kills work in flight: the guard stops a Pi at its next model
//! request, so a running turn ends where it is, and nothing new starts.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zeron_doc::SessionCommandPayload;
use zeron_proto::Chat;
use zeron_proto::orchestrator::{is_root_chat, tree_members};
use zeron_proto::orchestrator_project::{OrchestratorProject, ProjectUpdate, dollars};
use zeron_proto::usage::UsageQuery;

use crate::doc_host::DocHost;
use crate::sessions::SessionsEngine;
use crate::usage::UsageTracker;
use crate::workspace_host::WorkspaceHost;

const DIR: &str = "tree-policy";
const STATE_FILE_NAME: &str = "tree-policy-state.json";
/// The first check waits for the engine to settle; later ones follow this
/// often. A cap is a budget, not a hard stop: the guard and the gates in
/// `create_chat` and `send_message` cover the gap between two checks.
const FIRST_DELAY: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Overrides [`POLL_INTERVAL`], in seconds.
pub const ENV_POLL_SECS: &str = "ZERON_TREE_POLL_SECS";

// ───────────────────────────── the file the guard reads

/// Why a tree is halted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Halted {
    pub reason: String,
}

/// The file's shape, as the guard reads it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyFile {
    pub root_id: String,
    pub github_only_tests: bool,
    pub heavy_command_limit: Option<u8>,
    pub allowed_models: Vec<String>,
    /// Set while Zeron has the tree paused itself (the spending cap): the
    /// guard starts nothing and stops at the next model request.
    pub halted: Option<Halted>,
}

impl PolicyFile {
    pub fn of(project: &OrchestratorProject) -> Self {
        let reason = project.pause_reason.trim();
        Self {
            root_id: project.root_id.clone(),
            github_only_tests: project.policy.github_only_tests,
            heavy_command_limit: project.policy.heavy_command_limit,
            allowed_models: project.policy.allowed_models.clone(),
            halted: (project.paused && !reason.is_empty()).then(|| Halted {
                reason: reason.to_owned(),
            }),
        }
    }
}

/// The file of the orchestrator `root_id`.
pub fn file(data_dir: &Path, root_id: &str) -> PathBuf {
    data_dir
        .join(DIR)
        .join(format!("{}.json", crate::model_policy::stem(root_id)))
}

/// Make the orchestrator's file say what its project says, writing only when
/// it differs. Returns the file's path for `ZERON_TREE_POLICY`.
pub fn sync(data_dir: &Path, project: &OrchestratorProject) -> std::io::Result<PathBuf> {
    let path = file(data_dir, &project.root_id);
    let wanted = PolicyFile::of(project);
    let current = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<PolicyFile>(&text).ok());
    if current.as_ref() == Some(&wanted) {
        return Ok(path);
    }
    let dir = data_dir.join(DIR);
    std::fs::create_dir_all(&dir)?;
    let text = serde_json::to_vec(&wanted).map_err(std::io::Error::other)?;
    // Renamed into place so a guard never reads half a file. Windows will not
    // replace a file another process holds open: write it in place then.
    let tmp = dir.join(format!(
        ".{}.tmp",
        crate::model_policy::stem(&project.root_id)
    ));
    std::fs::write(&tmp, &text)?;
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(&path, &text)?;
    }
    Ok(path)
}

// ───────────────────────────── what a tree has spent

/// What a tree's sessions spent on this device, from the start of its
/// budget.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeSpend {
    pub spent_usd: f64,
    pub requests: u64,
    /// Requests with no price known: not in `spent_usd`.
    pub unpriced_requests: u64,
    /// The budget began here, Unix ms; 0 counts everything.
    pub since_ms: i64,
    pub cap_cents: Option<u64>,
    /// Every session of the tree ran on this device, so nothing is missing.
    pub complete: bool,
    /// Pi's files are still being read: the numbers may be one scan old.
    pub scanning: bool,
}

/// What the sessions of `project`'s tree spent here. A session's requests
/// are those of the Pi session its chat last ran (`harness_session_id`), its
/// sub-agents' and extensions' included wherever Pi's usage ledger names that
/// session; an earlier Pi session of a chat is not counted.
pub async fn tree_spend(
    usage: &UsageTracker,
    project: &OrchestratorProject,
    chats: &[Chat],
    device_id: &str,
) -> TreeSpend {
    let root = chats.iter().find(|chat| chat.id == project.root_id);
    let members = tree_members(chats, &project.root_id);
    let tree: Vec<&Chat> = root.into_iter().chain(members).collect();
    let sessions: Vec<String> = tree
        .iter()
        .filter(|chat| chat.device_id == device_id)
        .filter_map(|chat| chat.harness_session_id.as_deref())
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    let since_ms = project.policy.spend_since_ms;
    let report = usage
        .report(UsageQuery {
            since_ms: Some(since_ms).filter(|since| *since > 0),
            refresh: true,
            sessions: Some(sessions),
            ..Default::default()
        })
        .await;
    TreeSpend {
        spent_usd: report.models.iter().map(|row| row.cost_usd()).sum(),
        requests: report.models.iter().map(|row| row.requests).sum(),
        unpriced_requests: report.models.iter().map(|row| row.unpriced_requests).sum(),
        since_ms,
        cap_cents: project.policy.spend_cap_cents,
        complete: tree.iter().all(|chat| chat.device_id == device_id),
        scanning: report.scanning,
    }
}

// ───────────────────────────── the cap

/// Where a tree stands against its cap, remembered so a note is sent once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Stage {
    /// The budget this stage counts (`spend_since_ms`): a new cap is a new
    /// budget and starts over.
    since_ms: i64,
    warned: bool,
    halted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    None,
    /// Tell the orchestrator to wrap up.
    Warn,
    /// Pause the tree.
    Halt,
}

/// What `spent_usd` calls for. Each action is returned once per budget.
pub(crate) fn decide(stage: &mut Stage, project: &OrchestratorProject, spent_usd: f64) -> Action {
    let Some(cap) = project.policy.spend_cap_cents else {
        *stage = Stage::default();
        return Action::None;
    };
    if stage.since_ms != project.policy.spend_since_ms {
        *stage = Stage {
            since_ms: project.policy.spend_since_ms,
            ..Stage::default()
        };
    }
    // Resumed: the cap can pause it again.
    if !project.paused {
        stage.halted = false;
    }
    let spent = spent_usd * 100.0;
    let cap = cap as f64;
    if spent >= cap {
        let already = stage.halted || !project.pause_reason.is_empty();
        stage.halted = true;
        return if already { Action::None } else { Action::Halt };
    }
    let warn_at = cap * f64::from(project.policy.spend_warn_pct) / 100.0;
    if spent >= warn_at && !stage.warned {
        stage.warned = true;
        return Action::Warn;
    }
    Action::None
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct WatchState {
    stages: BTreeMap<String, Stage>,
}

/// Keeps the policy files current and holds trees to their caps.
#[derive(Clone)]
pub struct TreeWatcher {
    inner: Arc<Inner>,
}

struct Inner {
    data_dir: PathBuf,
    usage: UsageTracker,
    sessions: SessionsEngine,
    doc_host: DocHost,
    workspace: WorkspaceHost,
    state: Mutex<WatchState>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl TreeWatcher {
    pub fn new(
        data_dir: &Path,
        usage: UsageTracker,
        sessions: SessionsEngine,
        doc_host: DocHost,
        workspace: WorkspaceHost,
    ) -> Self {
        let state = std::fs::read_to_string(data_dir.join(STATE_FILE_NAME))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                data_dir: data_dir.to_path_buf(),
                usage,
                sessions,
                doc_host,
                workspace,
                state: Mutex::new(state),
                task: Mutex::new(None),
            }),
        }
    }

    /// Start checking (a no-op outside a tokio runtime).
    pub fn start(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let interval = std::env::var(ENV_POLL_SECS)
            .ok()
            .and_then(|secs| secs.trim().parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .map_or(POLL_INTERVAL, Duration::from_secs);
        let this = self.clone();
        let handle = runtime.spawn(async move {
            tokio::time::sleep(FIRST_DELAY.min(interval)).await;
            loop {
                this.tick().await;
                tokio::time::sleep(interval).await;
            }
        });
        *lock(&self.inner.task) = Some(handle);
    }

    pub fn shutdown(&self) {
        if let Some(task) = lock(&self.inner.task).take() {
            task.abort();
        }
    }

    /// One check of every orchestrator hosted here: refresh its policy file
    /// (an edit made on another device reaches this device's Pis within a
    /// tick), and when it has a cap, count its spend and act on it.
    pub async fn tick(&self) {
        let inner = &self.inner;
        let device = inner.doc_host.device_id().to_owned();
        let Ok(chats) = inner.workspace.read_chats() else {
            return;
        };
        let roots: Vec<&Chat> = chats
            .iter()
            .filter(|chat| is_root_chat(&chat.id) && !chat.archived && chat.device_id == device)
            .collect();
        let mut state = lock(&inner.state).clone();
        state
            .stages
            .retain(|root, _| roots.iter().any(|chat| chat.id == *root));
        for root in roots {
            let project = inner.workspace.orchestrator_project(&root.id);
            if let Err(error) = sync(&inner.data_dir, &project) {
                tracing::warn!(%error, root = %root.id, "tree policy: file write failed");
            }
            if project.policy.spend_cap_cents.is_none() {
                state.stages.remove(&root.id);
                continue;
            }
            let spend = tree_spend(&inner.usage, &project, &chats, &device).await;
            let stage = state.stages.entry(root.id.clone()).or_default();
            match decide(stage, &project, spend.spent_usd) {
                Action::None => {}
                Action::Warn => self.warn(&root.id, &project, &spend),
                Action::Halt => self.halt(&root.id, &project, &spend),
            }
        }
        let changed = *lock(&inner.state) != state;
        if changed {
            if let Err(error) = write_json(&inner.data_dir.join(STATE_FILE_NAME), &state) {
                tracing::warn!(%error, "tree policy: state write failed");
            }
            *lock(&inner.state) = state;
        }
    }

    /// At the warning percentage: tell the orchestrator to wrap up.
    fn warn(&self, root_id: &str, project: &OrchestratorProject, spend: &TreeSpend) {
        let cap = project.policy.spend_cap_cents.unwrap_or_default();
        let text = format!(
            "[Zeron spend cap] The sessions of this orchestrator have spent ${:.2} of the {} \
             spending cap ({}%). Wrap up: start no new sessions, let the running ones finish, \
             and report what is done and what is left. At the cap the tree is paused and its \
             sessions stop at their next model request.",
            spend.spent_usd,
            dollars(cap),
            project.policy.spend_warn_pct
        );
        let working = self
            .inner
            .sessions
            .session_status(root_id)
            .is_some_and(|session| session.status == zeron_proto::SessionStatus::Working);
        let sent = if working {
            self.inner
                .doc_host
                .queue_command(
                    root_id,
                    SessionCommandPayload::Steer {
                        prompt: text,
                        message_id: Some(crate::new_id()),
                    },
                )
                .map(|_| ())
        } else {
            self.inner.doc_host.notify_parent(root_id, &text, false)
        };
        match sent {
            Ok(()) => tracing::info!(root = %root_id, "tree policy: spending cap warning sent"),
            Err(error) => {
                tracing::warn!(%error, root = %root_id, "tree policy: spending cap warning failed");
            }
        }
    }

    /// At the cap: pause the tree, with the reason, and tell the guard.
    fn halt(&self, root_id: &str, project: &OrchestratorProject, spend: &TreeSpend) {
        let cap = project.policy.spend_cap_cents.unwrap_or_default();
        let reason = format!(
            "the spending cap of {} is reached (${:.2} spent)",
            dollars(cap),
            spend.spent_usd
        );
        tracing::warn!(root = %root_id, %reason, "tree policy: pausing the tree");
        let paused = self.inner.workspace.update_orchestrator_project(
            root_id,
            ProjectUpdate {
                paused: Some(true),
                pause_reason: Some(reason),
                ..Default::default()
            },
        );
        match paused {
            Ok(project) => {
                if let Err(error) = sync(&self.inner.data_dir, &project) {
                    tracing::warn!(%error, root = %root_id, "tree policy: file write failed");
                }
            }
            Err(error) => {
                tracing::warn!(%error, root = %root_id, "tree policy: pausing failed");
            }
        }
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::PiDirs;
    use zeron_proto::orchestrator_project::TreePolicy;

    fn project(policy: TreePolicy) -> OrchestratorProject {
        OrchestratorProject {
            root_id: "orchestrator-r".into(),
            policy,
            ..Default::default()
        }
    }

    fn capped(cents: u64, warn_pct: u8) -> OrchestratorProject {
        project(TreePolicy {
            spend_cap_cents: Some(cents),
            spend_since_ms: 100,
            spend_warn_pct: warn_pct,
            ..Default::default()
        })
    }

    #[test]
    fn the_file_follows_the_project_and_is_written_only_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = project(TreePolicy {
            github_only_tests: true,
            heavy_command_limit: Some(2),
            allowed_models: vec!["gpt-5.6".into()],
            ..Default::default()
        });
        let path = sync(dir.path(), &project).unwrap();
        assert_eq!(path, file(dir.path(), "orchestrator-r"));
        let read = || -> PolicyFile {
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
        };
        let first = read();
        assert!(first.github_only_tests && first.halted.is_none());
        assert_eq!(first.heavy_command_limit, Some(2));
        assert_eq!(first.allowed_models, ["gpt-5.6"]);
        // The same project leaves the file alone.
        let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
        sync(dir.path(), &project).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), stamp);
        // The user's own pause halts nothing; a pause with a reason does.
        project.paused = true;
        sync(dir.path(), &project).unwrap();
        assert!(read().halted.is_none());
        project.pause_reason = "the spending cap is reached".into();
        sync(dir.path(), &project).unwrap();
        assert_eq!(
            read().halted,
            Some(Halted {
                reason: "the spending cap is reached".into()
            })
        );
        // Resumed, the reason is gone with the pause.
        project.paused = false;
        project.pause_reason.clear();
        sync(dir.path(), &project).unwrap();
        assert!(read().halted.is_none());
    }

    #[test]
    fn an_odd_root_id_still_gets_a_file_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let odd = file(dir.path(), "orchestrator-a/b:c");
        assert_eq!(odd.parent(), Some(dir.path().join(DIR).as_path()));
        assert_ne!(odd, file(dir.path(), "orchestrator-a/b:d"));
    }

    #[test]
    fn the_cap_warns_once_halts_once_and_a_new_cap_starts_over() {
        let mut stage = Stage::default();
        let project = capped(1_000, 80);
        assert_eq!(decide(&mut stage, &project, 5.0), Action::None);
        assert_eq!(decide(&mut stage, &project, 8.0), Action::Warn);
        assert_eq!(decide(&mut stage, &project, 9.0), Action::None, "once");
        assert_eq!(decide(&mut stage, &project, 10.0), Action::Halt);
        assert_eq!(decide(&mut stage, &project, 12.0), Action::None, "once");
        // The pause is recorded in the project.
        let mut paused = project.clone();
        paused.paused = true;
        paused.pause_reason = "the spending cap is reached".into();
        assert_eq!(decide(&mut stage, &paused, 12.0), Action::None);
        // Resumed over the cap, it halts again; the user raising the cap
        // moves the budget and starts over.
        assert_eq!(decide(&mut stage, &project, 12.0), Action::Halt);
        let mut raised = capped(5_000, 80);
        raised.policy.spend_since_ms = 200;
        assert_eq!(decide(&mut stage, &raised, 1.0), Action::None);
        assert_eq!(
            decide(&mut stage, &raised, 40.0),
            Action::Warn,
            "warns again"
        );
        // No cap forgets everything.
        assert_eq!(
            decide(&mut stage, &project_without_cap(), 99.0),
            Action::None
        );
        assert_eq!(stage, Stage::default());
    }

    fn project_without_cap() -> OrchestratorProject {
        project(TreePolicy::default())
    }

    #[test]
    fn a_cap_that_a_user_pause_already_holds_is_still_recorded() {
        // The user paused, then the cap was reached: the pause gets its
        // reason so the guard halts.
        let mut stage = Stage::default();
        let mut user_paused = capped(1_000, 80);
        user_paused.paused = true;
        assert_eq!(decide(&mut stage, &user_paused, 10.0), Action::Halt);
    }

    fn session_line(id: &str, session: &str, cost: f64) -> [String; 2] {
        [
            serde_json::json!({"type":"session","version":3,"id":session,
                "timestamp":"2026-10-01T09:00:00Z","cwd":"/w"})
            .to_string(),
            serde_json::json!({
                "type":"message","id":id,"parentId":null,"timestamp":"2026-10-01T10:00:00.000Z",
                "message":{"role":"assistant","provider":"acme","model":"m-1","timestamp":1_000,
                    "content":[{"type":"text","text":"x"}],
                    "usage":{"input":10,"output":10,"cacheRead":100,"cacheWrite":5,"reasoning":4,
                        "totalTokens":125,
                        "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":cost}}}
            })
            .to_string(),
        ]
    }

    fn chat(id: &str, parent: Option<&str>, device: &str, session: Option<&str>) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": device, "title": null, "archived": false, "cwd": null,
            "branch": null, "checkoutId": null, "config": null, "lastMessagePreview": null,
            "lastMessageAt": null, "createdAt": "2026-01-01T00:00:00Z",
            "parentChatId": parent, "harnessSessionId": session,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn a_trees_spend_is_what_the_sessions_of_its_chats_spent() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let sessions = root.path().join("agent/sessions/--w--");
        std::fs::create_dir_all(&sessions).unwrap();
        for (name, id, session, cost) in [
            ("1.jsonl", "a1", "pi-root", 0.5),
            ("2.jsonl", "b1", "pi-member", 0.25),
            ("3.jsonl", "c1", "pi-other-tree", 4.0),
        ] {
            let lines = session_line(id, session, cost);
            std::fs::write(sessions.join(name), lines.join("\n") + "\n").unwrap();
        }
        let dirs = PiDirs {
            agent: root.path().join("agent"),
            sessions: vec![sessions],
            ledger: root.path().join("agent/usage-ledger"),
            workflows: root.path().join("workflows"),
        };
        let usage = UsageTracker::with_dirs(data.path(), move || dirs.clone());
        let chats = [
            chat("orchestrator-r", None, "dev-a", Some("pi-root")),
            chat("member", Some("orchestrator-r"), "dev-a", Some("pi-member")),
            chat("never-ran", Some("orchestrator-r"), "dev-a", None),
            chat("orchestrator-s", None, "dev-a", Some("pi-other-tree")),
        ];
        let mut project = project(TreePolicy::default());
        project.policy.spend_cap_cents = Some(500);
        let spend = tree_spend(&usage, &project, &chats, "dev-a").await;
        // The first report waits for the first scan.
        assert!((spend.spent_usd - 0.75).abs() < 1e-9, "{spend:?}");
        assert_eq!(spend.requests, 2);
        assert_eq!(spend.cap_cents, Some(500));
        assert!(spend.complete);
        // A member on another device ran there: this device cannot count it.
        let chats = [
            chat("orchestrator-r", None, "dev-a", Some("pi-root")),
            chat("member", Some("orchestrator-r"), "dev-b", Some("pi-member")),
        ];
        let spend = tree_spend(&usage, &project, &chats, "dev-a").await;
        assert!((spend.spent_usd - 0.5).abs() < 1e-9, "{spend:?}");
        assert!(!spend.complete);
    }
}
