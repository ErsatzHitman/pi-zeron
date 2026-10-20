//! Process ownership, including descendants that outlive their agent.
//!
//! Teardown inside a live engine is `Child::shutdown` / `Drop`. Surviving an
//! engine crash (SIGKILL, OOM) takes three layers:
//! 1. Every child carries `ZERON_ENGINE_PID`/`ZERON_ENGINE_START`, inherited by
//!    all descendants, so any of them is attributable to one engine instance.
//! 2. Linux: `PR_SET_PDEATHSIG` kills the direct child with the engine, even
//!    when its event loop is wedged. macOS has no equivalent and relies on the
//!    agent's own stdin-EOF shutdown plus layer 3. Windows needs neither: the
//!    job object is `KILL_ON_JOB_CLOSE` (`windows_process.rs`).
//! 3. At engine launch `process::reap_orphans` kills marked processes whose
//!    engine is gone, including descendants in their own session or group.
//!
//! An agent and everything it runs (cargo, node, tsc) also yields the CPU to
//! the app: below-normal priority on Windows, `nice` 10 on unix, both
//! inherited by descendants. `ZERON_AGENT_NICE` sets the niceness (0 is off).
use std::ops::{Deref, DerefMut};

use crate::process::orphans::{ENGINE_PID_ENV, ENGINE_START_ENV, current_marker};
use crate::process::{Child as ProcessChild, Command};

/// PDEATHSIG is tied to the spawning THREAD, not the process: start the
/// command with [`spawn`], or the child is SIGKILLed when the thread that
/// spawned it exits. Using `pre_exec` also makes std fall back from
/// posix_spawn to fork/exec.
pub(crate) fn configure(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    if agent_nice() > 0 {
        command.below_normal_priority();
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: the closure only calls async-signal-safe libc functions.
        let parent = unsafe { libc::getpid() };
        let nice = agent_nice();
        unsafe {
            command.pre_exec(move || {
                // Here, before exec, so every thread the agent starts inherits
                // it: Linux niceness is per thread.
                if nice > 0 {
                    let current = libc::getpriority(libc::PRIO_PROCESS, 0);
                    libc::setpriority(libc::PRIO_PROCESS, 0, current.max(nice));
                }
                if libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGKILL as libc::c_ulong,
                    0,
                    0,
                    0,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent may have died between fork and prctl.
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
    }
    let (pid, start) = current_marker();
    command
        .env(ENGINE_PID_ENV, pid.to_string())
        .env(ENGINE_START_ENV, start);
    for key in [
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SSE_PORT",
        "CLAUDE_AGENT_SDK_VERSION",
    ] {
        command.env_remove(key);
    }
}

/// Start a [`configure`]d command from the engine's spawner thread.
///
/// The kernel delivers PDEATHSIG when the spawning THREAD exits, and no tokio
/// worker is long-lived: a worker that blocks (`block_in_place`, which the
/// chat store's synchronous flush does) hands its role to a fresh thread and
/// retires, and a retired thread exits after its idle keep-alive. Each
/// retirement SIGKILLed the agents that thread had spawned, with nothing in
/// any log (Pi killed 35 minutes into a workflow, 2026-10-20). One thread
/// that lives as long as the engine makes the signal mean what it says: the
/// engine died.
pub(crate) fn spawn(command: Command) -> std::io::Result<ProcessChild> {
    #[cfg(target_os = "linux")]
    {
        type Job = Box<dyn FnOnce() + Send>;
        static SPAWNER: std::sync::OnceLock<std::sync::mpsc::Sender<Job>> =
            std::sync::OnceLock::new();
        let spawner = SPAWNER.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("zeron-agent-spawner".into())
                .spawn(move || {
                    for job in rx {
                        job();
                    }
                })
                .expect("agent spawner thread");
            tx
        });
        // The child registers with the caller's runtime (reaping, pipes).
        let runtime = tokio::runtime::Handle::try_current().ok();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut command = command;
        let job: Job = Box::new(move || {
            let _entered = runtime.as_ref().map(|runtime| runtime.enter());
            let _ = tx.send(command.spawn());
        });
        // fork/exec takes milliseconds; blocking the caller for it is what
        // `Command::spawn` does anyway.
        spawner
            .send(job)
            .map_err(|_| std::io::Error::other("the agent spawner thread is gone"))?;
        rx.recv()
            .map_err(|_| std::io::Error::other("the agent spawner thread is gone"))?
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut command = command;
        command.spawn()
    }
}

/// How much an agent's tree yields: `ZERON_AGENT_NICE` (0 to 19, 0 keeps
/// normal priority), else 10.
pub(crate) fn agent_nice() -> i32 {
    nice_from(std::env::var("ZERON_AGENT_NICE").ok().as_deref())
}

fn nice_from(value: Option<&str>) -> i32 {
    value
        .and_then(|value| value.trim().parse::<i32>().ok())
        .map_or(10, |nice| nice.clamp(0, 19))
}

pub(crate) struct Child {
    inner: ProcessChild,
    #[cfg(unix)]
    group: Option<i32>,
}

impl Child {
    pub(crate) fn new(inner: ProcessChild) -> Self {
        // macOS niceness is per process, so setting it right after spawn
        // keeps std's posix_spawn path (a pre_exec would force fork/exec).
        #[cfg(target_os = "macos")]
        if let Some(pid) = inner.id()
            && agent_nice() > 0
        {
            // SAFETY: plain syscall on our own child's pid.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, agent_nice()) };
        }
        Self {
            #[cfg(unix)]
            group: Some(-(inner.id().expect("newly spawned child") as i32)),
            inner,
        }
    }

    pub(crate) async fn shutdown(&mut self, grace: std::time::Duration) {
        // Everything the agent started, taken while it is still attached:
        // a command the agent ran in its own session (Devin does) is not in
        // the agent's process group and would outlive the runtime.
        #[cfg(unix)]
        let tree = match (self.group, self.inner.id()) {
            (Some(_), Some(pid)) => crate::process::descendants(pid).await,
            _ => Vec::new(),
        };
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            crate::send_signal(&group, crate::Signal::Term);
            // Pi uses detached bash groups and cleans them in its TERM handler.
            // Give descendants their grace even when the adapter exited first.
            let deadline = tokio::time::Instant::now() + grace;
            loop {
                let _ = self.inner.try_wait();
                // SAFETY: signal 0 only checks the private process group.
                if unsafe { libc::kill(group, 0) } != 0 {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    crate::send_signal(&group, crate::Signal::Kill);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        crate::shutdown_child(&mut self.inner, grace).await;
        #[cfg(unix)]
        crate::process::terminate_tree(&tree, grace).await;
    }

    pub(crate) fn request_group_shutdown(&self) {
        #[cfg(unix)]
        if let Some(group) = self.group {
            crate::send_signal(&group, crate::Signal::Term);
        }
    }

    pub(crate) fn terminate_group(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            crate::send_signal(&group, crate::Signal::Kill);
        }
    }
}

impl Deref for Child {
    type Target = ProcessChild;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // Tokio's kill_on_drop covers the direct child. Keep the group id
        // even after wait() has reaped it, so descendants cannot escape cleanup.
        self.terminate_group();
    }
}

#[cfg(test)]
mod nice_tests {
    use super::nice_from;

    #[test]
    fn agents_yield_by_default_and_the_variable_tunes_or_turns_it_off() {
        assert_eq!(nice_from(None), 10);
        assert_eq!(nice_from(Some("0")), 0);
        assert_eq!(nice_from(Some(" 5 ")), 5);
        assert_eq!(nice_from(Some("40")), 19);
        assert_eq!(nice_from(Some("-3")), 0, "never raised above normal");
        assert_eq!(nice_from(Some("high")), 10);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn an_agent_outlives_the_thread_that_asked_for_it() {
        // Spawned from a thread that then exits: with PDEATHSIG bound to the
        // caller, the kernel would SIGKILL the child the moment it returns.
        let runtime = tokio::runtime::Handle::current();
        let mut child = std::thread::spawn(move || {
            let _entered = runtime.enter();
            let mut command = Command::new("sleep");
            command.arg("2");
            configure(&mut command);
            spawn(command).unwrap()
        })
        .join()
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            child.try_wait().unwrap().is_none(),
            "the agent died with the thread that spawned it"
        );
        child.kill().await.unwrap();
    }

    #[test]
    fn acp_environment_removes_nested_claude_markers() {
        let mut command = Command::new("unused");
        configure(&mut command);
        let removed: Vec<_> = command
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_str().unwrap())
            .collect();
        assert_eq!(
            removed,
            vec![
                "CLAUDECODE",
                "CLAUDE_AGENT_SDK_VERSION",
                "CLAUDE_CODE_ENTRYPOINT",
                "CLAUDE_CODE_SSE_PORT"
            ]
        );
    }

    #[test]
    fn configure_exports_engine_identity_and_scrubs_claude_markers() {
        let mut command = Command::new("unused");
        configure(&mut command);
        let env = |key: &str| {
            command
                .as_std()
                .get_envs()
                .find(|(k, _)| k.to_str() == Some(key))
                .and_then(|(_, v)| v.map(|v| v.to_str().unwrap().to_string()))
        };
        assert_eq!(
            env("ZERON_ENGINE_PID"),
            Some(std::process::id().to_string())
        );
        assert!(!env("ZERON_ENGINE_START").unwrap().is_empty());
        assert_eq!(env("CLAUDECODE"), None);
    }
}
