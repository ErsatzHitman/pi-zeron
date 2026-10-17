//! Launch-time reaper for Pi processes a previous engine left behind.
//!
//! Every Pi child carries `ZERON_ENGINE_PID` + `ZERON_ENGINE_START` (the
//! spawning engine's pid and process start time), inherited by all of its
//! descendants, including detached shells and sub-Pis. At engine launch any
//! process carrying a marker whose engine is no longer running is an orphan of
//! a crashed or SIGKILLed engine and is terminated. Identity is the env marker
//! plus engine liveness, never argv: another live engine's Pi, or another
//! user's, is untouched. The start time defeats pid reuse.
//!
//! Scope caveat: the marker is inherited by anything a Pi tool deliberately
//! detached (a `nohup` dev server), so after an engine crash those are reaped
//! at the next launch too. Processes the herdr server starts in its own panes
//! do not inherit the env and are not touched.

pub(crate) const ENGINE_PID_ENV: &str = "ZERON_ENGINE_PID";
pub(crate) const ENGINE_START_ENV: &str = "ZERON_ENGINE_START";
/// Recorded when the engine's own start time could not be read; liveness then
/// falls back to the pid alone (never kill on uncertainty).
const UNKNOWN_START: &str = "unknown";

/// This engine's identity, computed once: the pid and a single-token start time.
pub(crate) fn current_marker() -> (u32, String) {
    static MARKER: std::sync::OnceLock<(u32, String)> = std::sync::OnceLock::new();
    MARKER
        .get_or_init(|| {
            let pid = std::process::id();
            let start = process_start(pid).unwrap_or_else(|| UNKNOWN_START.to_string());
            (pid, start)
        })
        .clone()
}

/// Field 22 (`starttime`, clock ticks since boot) of a `/proc/<pid>/stat` line.
/// `comm` may contain spaces and parens, so parse after the LAST `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat_starttime(stat: &str) -> Option<String> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // `rest` begins at field 3 (state); starttime is field 22 -> index 19.
    rest.split_whitespace().nth(19).map(str::to_string)
}

/// A start time that is stable for a pid's lifetime and differs across pid
/// reuse, as a single whitespace-free token. `None` when unreadable.
#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<String> {
    parse_stat_starttime(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

#[cfg(target_os = "macos")]
fn process_start(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = text.split_whitespace().collect();
    // Spaces would split the env token in `ps eww` output.
    (!parts.is_empty()).then(|| parts.join("_"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_start(_pid: u32) -> Option<String> {
    None
}

/// Whether the engine that stamped a marker is still running. Unreadable
/// identity counts as alive.
#[cfg(unix)]
fn live_engine(pid: u32, recorded_start: &str) -> bool {
    // SAFETY: signal 0 only probes for existence.
    if unsafe { libc::kill(pid as i32, 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        return false;
    }
    if recorded_start == UNKNOWN_START {
        return true;
    }
    match process_start(pid) {
        Some(now) => now == recorded_start,
        None => true,
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Marked {
    pid: i32,
    engine_pid: u32,
    engine_start: String,
}

/// Pull the marker out of `KEY=VALUE` env entries.
#[cfg(unix)]
fn marker_from_entries<'a>(entries: impl Iterator<Item = &'a str>) -> Option<(u32, String)> {
    let (mut pid, mut start) = (None, None);
    for entry in entries {
        if let Some(v) = entry.strip_prefix("ZERON_ENGINE_PID=") {
            pid = v.parse::<u32>().ok();
        } else if let Some(v) = entry.strip_prefix("ZERON_ENGINE_START=") {
            start = Some(v.to_string());
        }
    }
    Some((pid?, start?))
}

#[cfg(target_os = "linux")]
fn marker_of(pid: i32) -> Option<(u32, String)> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    marker_from_entries(
        environ
            .split(|b| *b == 0)
            .filter_map(|entry| std::str::from_utf8(entry).ok()),
    )
}

/// Unreadable environments (other users, races with exit) are skipped.
#[cfg(target_os = "linux")]
fn scan_marked() -> Vec<Marked> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(|pid| {
            let (engine_pid, engine_start) = marker_of(pid)?;
            Some(Marked {
                pid,
                engine_pid,
                engine_start,
            })
        })
        .collect()
}

/// One `ps eww` line: `<pid> <command and env...>`; the env follows the
/// command as space separated `KEY=VALUE` tokens.
#[cfg(unix)]
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_ps_line(line: &str) -> Option<Marked> {
    let line = line.trim_start();
    let (pid, rest) = line.split_once(char::is_whitespace)?;
    let (engine_pid, engine_start) = marker_from_entries(rest.split(' '))?;
    Some(Marked {
        pid: pid.parse().ok()?,
        engine_pid,
        engine_start,
    })
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ps_marked(args: &[&str]) -> Vec<Marked> {
    let Ok(out) = std::process::Command::new("ps")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_ps_line)
        .collect()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn scan_marked() -> Vec<Marked> {
    ps_marked(&["eww", "-A", "-o", "pid=,command="])
}

#[cfg(all(unix, not(target_os = "linux")))]
fn marker_of(pid: i32) -> Option<(u32, String)> {
    ps_marked(&["eww", "-p", &pid.to_string(), "-o", "pid=,command="])
        .into_iter()
        .find(|m| m.pid == pid)
        .map(|m| (m.engine_pid, m.engine_start))
}

/// This process's ancestors (never signalled even if they carry a marker).
#[cfg(unix)]
fn ancestors() -> Vec<i32> {
    let mut chain = Vec::new();
    // SAFETY: getppid cannot fail.
    let mut pid = unsafe { libc::getppid() };
    while pid > 1 && !chain.contains(&pid) && chain.len() < 64 {
        chain.push(pid);
        #[cfg(target_os = "linux")]
        {
            let next = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| {
                    s[s.rfind(')')? + 1..]
                        .split_whitespace()
                        .nth(1)?
                        .parse::<i32>()
                        .ok()
                });
            match next {
                Some(next) => pid = next,
                None => break,
            }
        }
        #[cfg(not(target_os = "linux"))]
        break;
    }
    chain
}

/// Terminate Pi processes orphaned by a previous engine. Synchronous; costs a
/// process scan unless orphans exist (then at most ~350 ms). Returns how many
/// processes were targeted. Call before any session recovery re-dispatches work.
#[cfg(unix)]
pub fn reap_orphans() -> usize {
    reap_orphans_with(live_engine)
}

/// [`reap_orphans`] with injected engine liveness, so tests never touch a real engine.
#[cfg(unix)]
#[doc(hidden)]
pub fn reap_orphans_with(is_live: impl Fn(u32, &str) -> bool) -> usize {
    let own = std::process::id() as i32;
    let parents = ancestors();
    let orphans: Vec<Marked> = scan_marked()
        .into_iter()
        .filter(|m| m.pid > 1 && m.pid != own && !parents.contains(&m.pid))
        .filter(|m| !is_live(m.engine_pid, &m.engine_start))
        .collect();
    if orphans.is_empty() {
        return 0;
    }
    let targets: Vec<i32> = orphans
        .iter()
        .map(|m| super::signal_target_for(m.pid))
        .collect();
    for &target in &targets {
        crate::send_signal(&target, crate::Signal::Term);
    }
    // SAFETY: signal 0 only probes for existence.
    let alive = |target: i32| unsafe { libc::kill(target, 0) } == 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
    while targets.iter().any(|&t| alive(t)) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    for (m, &target) in orphans.iter().zip(&targets) {
        // Narrow the pid-reuse window: only kill what still carries the marker.
        let same = marker_of(m.pid)
            .is_some_and(|(pid, start)| pid == m.engine_pid && start == m.engine_start);
        if alive(target) && same {
            crate::send_signal(&target, crate::Signal::Kill);
        }
    }
    tracing::warn!(
        count = orphans.len(),
        "reaped orphaned Pi processes from a previous engine"
    );
    orphans.len()
}

/// Windows needs no reaper: the Pi job object is created with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (windows_process.rs), so the OS kills it
/// with the engine.
#[cfg(not(unix))]
pub fn reap_orphans() -> usize {
    0
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    /// Ends the fixture process (and its group) even when an assertion fails.
    struct Reap(Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            // SAFETY: SIGKILL to the fixture's own pid / group.
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGKILL);
                libc::kill(self.0.id() as i32, libc::SIGKILL);
            }
            let _ = self.0.wait();
        }
    }

    fn sleeper(env: Option<(String, String)>, group: bool, shell: bool) -> Reap {
        let mut command = if shell {
            let mut c = Command::new("sh");
            c.args(["-c", "sleep 60 & wait"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("60");
            c
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marked = env.is_some();
        if let Some((pid, start)) = env {
            command
                .env(ENGINE_PID_ENV, pid)
                .env(ENGINE_START_ENV, start);
        }
        if group {
            std::os::unix::process::CommandExt::process_group(&mut command, 0);
        }
        let child = Reap(command.spawn().unwrap());
        // /proc/<pid>/environ only shows the new env once the child has exec'd.
        for _ in 0..300 {
            if !marked || marker_of(child.0.id() as i32).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if shell {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        child
    }

    fn dead_pid() -> u32 {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn gone(child: &mut Reap) -> bool {
        for _ in 0..100 {
            if child.0.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    fn survives(child: &mut Reap) -> bool {
        std::thread::sleep(std::time::Duration::from_millis(400));
        child.0.try_wait().unwrap().is_none()
    }

    #[test]
    fn stat_parser_handles_parens_in_comm() {
        let stat = "123 ((a) (b)) S 1 123 123 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10 18446744073709551615";
        assert_eq!(parse_stat_starttime(stat).as_deref(), Some("987654"));
        assert!(parse_stat_starttime("garbage").is_none());
    }

    #[test]
    fn ps_line_parser_finds_marker_in_environment() {
        let line = "  4242 /bin/sleep 60 HOME=/x ZERON_ENGINE_PID=77 ZERON_ENGINE_START=Mon_Jan_1_00:00:00_2024 PATH=/bin";
        assert_eq!(
            parse_ps_line(line),
            Some(Marked {
                pid: 4242,
                engine_pid: 77,
                engine_start: "Mon_Jan_1_00:00:00_2024".into()
            })
        );
        assert!(parse_ps_line("1 /sbin/init HOME=/").is_none());
    }

    /// Reap only this test's marker so parallel tests do not steal each other's
    /// targets (real liveness, scoped by the start tag).
    fn reap_tagged(tag: &str) -> usize {
        reap_orphans_with(|pid, start| start != tag || live_engine(pid, start))
    }

    #[test]
    fn reaps_marked_process_of_dead_engine() {
        let mut child = sleeper(Some((dead_pid().to_string(), "dead".into())), false, false);
        assert!(reap_tagged("dead") >= 1);
        assert!(gone(&mut child));
    }

    #[test]
    fn spares_process_of_live_engine() {
        let (pid, start) = current_marker();
        let mut child = sleeper(Some((pid.to_string(), start)), false, false);
        reap_orphans();
        assert!(survives(&mut child));
    }

    #[test]
    fn reaps_when_pid_was_reused() {
        let (pid, _) = current_marker();
        let mut child = sleeper(Some((pid.to_string(), "reused".into())), false, false);
        assert!(reap_tagged("reused") >= 1);
        assert!(gone(&mut child));
    }

    #[test]
    fn ignores_unmarked_processes() {
        let mut child = sleeper(None, false, false);
        reap_orphans();
        assert!(survives(&mut child));
    }

    #[test]
    fn reaps_whole_process_group_of_marked_leader() {
        let mut leader = sleeper(Some((dead_pid().to_string(), "group".into())), true, true);
        assert!(reap_tagged("group") >= 1);
        assert!(gone(&mut leader));
        // The shell's `sleep` shared the group and died with it (a zombie
        // awaiting an init that never reaps still counts as dead).
        let pgrp = leader.0.id().to_string();
        let live_member = std::fs::read_dir("/proc").unwrap().flatten().any(|e| {
            let stat = std::fs::read_to_string(e.path().join("stat")).unwrap_or_default();
            let rest: Vec<&str> = stat
                .rfind(')')
                .map(|i| stat[i + 1..].split_whitespace().collect())
                .unwrap_or_default();
            rest.get(2) == Some(&pgrp.as_str()) && rest.first() != Some(&"Z")
        });
        assert!(!live_member);
    }
}
