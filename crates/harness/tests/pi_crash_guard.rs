//! An engine that dies without running any Drop (SIGKILL/OOM) must not leave
//! Pi processes behind: the direct Pi dies with it (stdin EOF / PDEATHSIG), and
//! the next launch reaps what neither can reach (a tool in Pi's group).
#![cfg(all(feature = "native-fixture", target_os = "linux"))]
use futures::StreamExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use zeron_harness::{CancellationToken, Harness, PiHarness, RunControls};
use zeron_proto::{AgentEvent, RunRequest, SandboxLevel};

/// The two scenarios share the reaper's global view of the process table.
static SERIAL: Mutex<()> = Mutex::new(());

fn harness() -> PiHarness {
    PiHarness::new()
        .with_executable(env!("CARGO_BIN_EXE_harness-pi-fixture"))
        .with_agent_dir(Path::new(env!("CARGO_TARGET_TMPDIR")).join("pi-guard-agent"))
        .with_graces(Duration::from_millis(100), Duration::from_millis(100))
}

fn request(cwd: &Path, prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: None,
        attachments: vec![],
        worktree: None,
        mcp: None,
    }
}

/// The "engine": runs a Pi to the point its tool is alive, then dies by
/// SIGKILL so no destructor runs. Only meaningful when re-executed by
/// `spawn_killed_engine`.
#[tokio::test]
#[ignore = "re-executed by the crash-guard scenarios"]
async fn crash_guard_parent_role() {
    let Ok(dir) = std::env::var("PI_GUARD_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let (tx, rx) = mpsc::channel(8);
    let controls = RunControls {
        realtime: None,
        execution_lease: None,
        steering: rx,
        interrupt: CancellationToken::new(),
        request_input: Box::new(|_| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(vec![]);
            rx
        }),
        turn: Default::default(),
        extension_ui: Default::default(),
    };
    let mut stream = harness()
        .with_session_store(dir.join("index"))
        .run(request(&dir, "tree"), controls)
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        if matches!(event.unwrap(), AgentEvent::TextDelta { text } if text == "tree ready") {
            let _keep = &tx;
            // SAFETY: deliberately dies without unwinding or running Drop.
            unsafe { libc::raise(libc::SIGKILL) };
        }
    }
}

fn alive(pid: &str) -> bool {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    !stat.is_empty() && stat.split_whitespace().nth(2) != Some("Z")
}

fn wait_dead(pid: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
}

/// Runs the killed engine to completion (reaping it, so its pid is truly
/// gone) and returns `(pi pid, tool pid)`.
fn spawn_killed_engine(dir: &Path) -> (String, String) {
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "crash_guard_parent_role",
            "--ignored",
            "--nocapture",
        ])
        .env("PI_GUARD_DIR", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "the engine role must die by SIGKILL");
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
    (read("pi.pid"), read("tool.pid"))
}

/// Ends the tool even when an assertion fails first.
struct Reap(String);
impl Drop for Reap {
    fn drop(&mut self) {
        if let Ok(pid) = self.0.parse::<i32>() {
            // SAFETY: SIGKILL to the fixture's own tool.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

#[test]
fn pdeathsig_kills_child_when_spawning_thread_exits_or_parent_dies() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let (pi, tool) = spawn_killed_engine(dir.path());
    let _tool = Reap(tool.clone());
    assert!(
        wait_dead(&pi, Duration::from_secs(3)),
        "the direct Pi must die with its engine"
    );
    assert!(
        alive(&tool),
        "a tool in Pi's group is beyond EOF and PDEATHSIG: only the launch reaper reaches it"
    );
}

#[test]
fn launch_reaper_kills_orphans_of_a_killed_engine() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let (_pi, tool) = spawn_killed_engine(dir.path());
    let _tool = Reap(tool.clone());
    assert!(alive(&tool));
    assert!(zeron_harness::process::reap_orphans() >= 1);
    assert!(
        wait_dead(&tool, Duration::from_secs(2)),
        "the orphaned tool must be reaped"
    );
    assert_eq!(
        zeron_harness::process::reap_orphans(),
        0,
        "reaping is idempotent"
    );
}
