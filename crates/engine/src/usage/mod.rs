//! The usage tracker: tokens and spend for every Pi provider and model on
//! this device, from Pi's session files, its usage ledger (every LLM call a
//! Pi process made, children and extensions included) and, for the time
//! before the ledger, what subagents, the advisor and workflows logged.
//! Scans run in the background and only read what was appended since the
//! last one; the index lives in `<data_dir>/usage/`. See
//! docs/research/usage-tracker.md.
mod price;
mod report;
mod scan;

use scan::{FileKind, FileState};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeron_proto::usage::{UsageQuery, UsageReport};

/// A report this fresh is served without rescanning.
const FRESH: Duration = Duration::from_secs(60);
/// How long the first report waits for the first scan before answering
/// with what it has.
const FIRST_SCAN_WAIT: Duration = Duration::from_secs(5);
const INDEX_VERSION: u32 = 3;

/// Where Pi keeps sessions: `PI_CODING_AGENT_SESSION_DIR`, the agent
/// directory's `sessions`, and a `sessionDir` from its global settings.
/// The usage ledger is the agent directory's `usage-ledger` unless
/// `PI_USAGE_LEDGER_DIR` moves it; workflow journals live under
/// `~/.pi/workflows/projects`.
#[derive(Debug, Clone, PartialEq)]
pub struct PiDirs {
    pub agent: PathBuf,
    pub sessions: Vec<PathBuf>,
    pub ledger: PathBuf,
    pub workflows: PathBuf,
}

impl PiDirs {
    pub fn detect() -> Self {
        let env = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = crate::repos::home_dir();
        let agent = env("PI_CODING_AGENT_DIR").unwrap_or_else(|| home.join(".pi").join("agent"));
        let mut sessions = vec![];
        sessions.extend(env("PI_CODING_AGENT_SESSION_DIR"));
        sessions.push(agent.join("sessions"));
        let settings: serde_json::Value = std::fs::read(agent.join("settings.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        if let Some(dir) = settings["sessionDir"].as_str() {
            if let Some(rest) = dir.strip_prefix("~/") {
                sessions.push(home.join(rest));
            } else if Path::new(dir).is_absolute() {
                sessions.push(dir.into());
            }
        }
        let mut unique = vec![];
        for dir in sessions {
            if !unique.contains(&dir) {
                unique.push(dir);
            }
        }
        Self {
            ledger: env("PI_USAGE_LEDGER_DIR").unwrap_or_else(|| agent.join("usage-ledger")),
            workflows: home.join(".pi").join("workflows").join("projects"),
            agent,
            sessions: unique,
        }
    }

    /// Every file the tracker reads, with how to read it.
    fn files(&self) -> Vec<(PathBuf, FileKind)> {
        let mut files: Vec<_> = scan::session_files(&self.sessions)
            .into_iter()
            .filter(|path| !path.starts_with(&self.ledger))
            .map(|path| (path, FileKind::Session))
            .collect();
        files.extend(
            scan::session_files(std::slice::from_ref(&self.ledger))
                .into_iter()
                .map(|path| (path, FileKind::Ledger)),
        );
        files.extend(
            scan::journal_files(&self.workflows)
                .into_iter()
                .map(|path| (path, FileKind::Journal)),
        );
        files
    }
}

#[derive(Serialize, Deserialize)]
struct Index {
    version: u32,
    scanned_at_ms: Option<i64>,
    files: BTreeMap<String, FileState>,
}

#[derive(Default)]
struct State {
    loaded: bool,
    files: BTreeMap<String, Arc<FileState>>,
    book: price::PriceBook,
    scan: Option<tokio::sync::watch::Receiver<bool>>,
    scanned_at: Option<Instant>,
    scanned_at_ms: Option<i64>,
}

struct Inner {
    index: PathBuf,
    dirs: Box<dyn Fn() -> PiDirs + Send + Sync>,
    state: Mutex<State>,
}

/// Marks a scan finished when it ends, even by panicking, so the next
/// report can start another.
struct Finished(tokio::sync::watch::Sender<bool>);
impl Drop for Finished {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

/// Cheap to clone; every clone shares one index.
#[derive(Clone)]
pub struct UsageTracker {
    inner: Arc<Inner>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl UsageTracker {
    pub fn new(data_dir: &Path) -> Self {
        Self::with_dirs(data_dir, PiDirs::detect)
    }

    /// Pi's directories are found again on every scan, so a moved session
    /// directory is followed without a restart.
    pub fn with_dirs(data_dir: &Path, dirs: impl Fn() -> PiDirs + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(Inner {
                index: data_dir.join("usage").join("usage-index-v1.json"),
                dirs: Box::new(dirs),
                state: Mutex::new(State::default()),
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The report for `query`. A stale or requested rescan starts in the
    /// background and the reply says so; the very first one is waited for
    /// briefly.
    pub async fn report(&self, query: UsageQuery) -> UsageReport {
        let (scan, first) = {
            let mut state = self.state();
            let running = state.scan.as_ref().is_some_and(|rx| !*rx.borrow());
            let stale = state.scanned_at.is_none_or(|at| at.elapsed() > FRESH);
            if !running && (query.refresh || stale) {
                let (tx, rx) = tokio::sync::watch::channel(false);
                state.scan = Some(rx);
                let tracker = self.clone();
                tokio::task::spawn_blocking(move || {
                    let _finished = Finished(tx);
                    tracker.scan();
                });
            }
            (state.scan.clone(), state.scanned_at.is_none())
        };
        if first && let Some(mut rx) = scan {
            let _ = tokio::time::timeout(FIRST_SCAN_WAIT, rx.wait_for(|done| *done)).await;
        }
        self.current(&query)
    }

    fn current(&self, query: &UsageQuery) -> UsageReport {
        let (files, book, scanning, scanned_at_ms) = {
            let state = self.state();
            (
                state.files.clone(),
                state.book.clone(),
                state.scan.as_ref().is_some_and(|rx| !*rx.borrow()),
                state.scanned_at_ms,
            )
        };
        let files: Vec<(&str, &FileState)> = files
            .iter()
            .map(|(path, file)| (path.as_str(), file.as_ref()))
            .collect();
        UsageReport {
            scanning,
            scanned_at_ms,
            ..report::build(&files, &book, query)
        }
    }

    /// Read what changed since the last scan, then save the index.
    fn scan(&self) {
        if !self.state().loaded {
            let index = std::fs::read(&self.inner.index)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Index>(&bytes).ok())
                .filter(|index| index.version == INDEX_VERSION);
            let mut state = self.state();
            state.loaded = true;
            if let Some(index) = index {
                state.scanned_at_ms = index.scanned_at_ms;
                state.files = index
                    .files
                    .into_iter()
                    .map(|(path, file)| (path, Arc::new(file)))
                    .collect();
            }
        }
        let dirs = (self.inner.dirs)();
        let previous = self.state().files.clone();
        let mut files = BTreeMap::new();
        let mut changed = false;
        for (path, kind) in dirs.files() {
            let key = path.to_string_lossy().into_owned();
            let prev = previous.get(&key);
            let scanned = match kind {
                FileKind::Journal => scan::scan_journal(&path, prev.map(Arc::as_ref)),
                kind => scan::scan_file(&path, prev.map(Arc::as_ref), kind),
            };
            match scanned {
                Ok(Some(file)) => {
                    changed = true;
                    files.insert(key, Arc::new(file));
                }
                Ok(None) => {
                    files.insert(key, prev.cloned().unwrap_or_default());
                }
                Err(error) => {
                    tracing::debug!(%error, path = %path.display(), "usage scan skipped a file");
                    if let Some(prev) = prev {
                        files.insert(key, prev.clone());
                    }
                }
            }
        }
        // Deleted sessions still happened.
        for (key, file) in previous {
            if !files.contains_key(&key) {
                let file = if file.gone {
                    file
                } else {
                    changed = true;
                    Arc::new(FileState {
                        gone: true,
                        ..(*file).clone()
                    })
                };
                files.insert(key, file);
            }
        }
        let book = price::PriceBook::load(&dirs.agent, &dirs.ledger);
        let scanned_at_ms = now_ms();
        {
            let mut state = self.state();
            state.files = files.clone();
            state.book = book;
            state.scanned_at = Some(Instant::now());
            state.scanned_at_ms = Some(scanned_at_ms);
        }
        if changed && let Err(error) = self.save(&files, scanned_at_ms) {
            tracing::warn!(%error, "usage index not saved");
        }
    }

    fn save(
        &self,
        files: &BTreeMap<String, Arc<FileState>>,
        scanned_at_ms: i64,
    ) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct IndexRef<'a> {
            version: u32,
            scanned_at_ms: Option<i64>,
            files: BTreeMap<&'a str, &'a FileState>,
        }
        let bytes = serde_json::to_vec(&IndexRef {
            version: INDEX_VERSION,
            scanned_at_ms: Some(scanned_at_ms),
            files: files
                .iter()
                .map(|(path, file)| (path.as_str(), file.as_ref()))
                .collect(),
        })?;
        let path = &self.inner.index;
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        let staging = path.with_extension(format!("{}.partial", uuid::Uuid::new_v4()));
        let result = std::fs::write(&staging, bytes).and_then(|()| std::fs::rename(&staging, path));
        if result.is_err() {
            let _ = std::fs::remove_file(&staging);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scan::tests::{assistant, header, ledger_call};

    fn tracker(root: &Path, data: &Path) -> UsageTracker {
        let dirs = PiDirs {
            agent: root.join("agent"),
            sessions: vec![root.join("agent/sessions")],
            ledger: root.join("agent/usage-ledger"),
            workflows: root.join("workflows/projects"),
        };
        UsageTracker::with_dirs(data, move || dirs.clone())
    }

    fn write(path: &Path, lines: &[String]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    #[tokio::test]
    async fn the_first_report_scans_and_later_ones_pick_up_new_sessions_and_survive_a_restart() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let sessions = root.path().join("agent/sessions/--w--");
        write(
            &sessions.join("1_a.jsonl"),
            &[
                header("2026-10-01T09:00:00Z"),
                assistant("a1", "anthropic", "claude-opus-5-5", 1_000, 10, 0.5),
            ],
        );
        let usage = tracker(root.path(), data.path());
        let report = usage.report(UsageQuery::default()).await;
        assert_eq!(report.models.len(), 1);
        assert!(!report.scanning);
        assert!(report.scanned_at_ms.is_some());

        // A provider logged in later appears on the next refresh.
        write(
            &sessions.join("2_b.jsonl"),
            &[
                header("2026-10-02T09:00:00Z"),
                assistant("b1", "acme-llm", "x-1", 2_000, 10, 0.0),
            ],
        );
        let mut report = usage
            .report(UsageQuery {
                refresh: true,
                ..Default::default()
            })
            .await;
        for _ in 0..100 {
            if !report.scanning {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            report = usage.report(UsageQuery::default()).await;
        }
        let providers: Vec<_> = report.models.iter().map(|m| m.provider.as_str()).collect();
        assert_eq!(providers, ["anthropic", "acme-llm"]);

        // A restarted engine starts from the saved index, and a deleted
        // session's usage still counts.
        std::fs::remove_file(sessions.join("1_a.jsonl")).unwrap();
        let restarted = tracker(root.path(), data.path());
        let report = restarted.report(UsageQuery::default()).await;
        assert_eq!(report.models.len(), 2, "{:?}", report.models);
        assert_eq!(report.files, 1);
    }

    #[tokio::test]
    async fn ledger_calls_and_workflow_journals_are_read_beside_sessions() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        write(
            &root.path().join("agent/sessions/--w--/1_a.jsonl"),
            &[
                header("2026-10-01T09:00:00Z"),
                assistant("a1", "anthropic", "claude-opus-5-5", 1_000, 10, 0.5),
            ],
        );
        // A subagent ran without a session file; only the ledger saw it.
        write(
            &root.path().join("agent/usage-ledger/2026-10.jsonl"),
            &[
                serde_json::json!({"v":1,"t":"start","ts":1_790_900_000_000_i64,"pid":1})
                    .to_string(),
                ledger_call("1-a-1", "main", "anthropic", "claude-opus-5-5", 1_000, 10),
                ledger_call("2-a-1", "subagent", "openai", "gpt-5.6", 5_000, 30),
            ],
        );
        write(
            &root.path().join("workflows/projects/w-1/wf_1.json"),
            &[
                serde_json::json!({"runId":"wf_1","agents":[{"callId":"wf_1:0",
                "model":"openai/gpt-5.6","startedAt":"2026-10-01T10:00:00.000Z",
                "usage":{"input":7,"output":1,"total":8,"cost":0.1,"cacheRead":0,"cacheWrite":0}}]})
                .to_string(),
            ],
        );
        let report = tracker(root.path(), data.path())
            .report(UsageQuery::default())
            .await;
        let mut surfaces: Vec<_> = report
            .surfaces
            .iter()
            .map(|row| (row.surface.as_str(), row.requests))
            .collect();
        surfaces.sort();
        assert_eq!(surfaces, [("main", 1), ("subagent", 1), ("workflow", 1)]);
        assert_eq!(report.coverage.ledger_duplicates, 1);
        // The journal's run came before the ledger began.
        assert_eq!(report.coverage.ledger_from_ms, Some(1_790_900_000_000));
        assert_eq!(report.coverage.backfilled_requests, 1);
        assert_eq!(report.files, 1);
    }

    #[test]
    fn pi_dirs_include_the_agent_dirs_sessions() {
        let dirs = PiDirs::detect();
        assert!(dirs.sessions.contains(&dirs.agent.join("sessions")));
        if std::env::var_os("PI_USAGE_LEDGER_DIR").is_none() {
            assert_eq!(dirs.ledger, dirs.agent.join("usage-ledger"));
        }
        assert!(dirs.workflows.ends_with("projects"));
    }
}
