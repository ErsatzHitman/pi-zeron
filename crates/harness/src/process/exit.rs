//! What a process's exit status means, in words a person can act on.
//!
//! "killed by signal 9" sends people hunting. SIGKILL from outside the engine
//! is most often the out-of-memory killer, so the message says so, and on
//! Linux it checks the evidence: the kernel counts OOM kills per cgroup
//! (`memory.events`), and a count that rose while the child ran is proof.

use std::path::PathBuf;
use std::process::ExitStatus;

/// The conventional name of a fatal signal. Only numbers Linux and macOS
/// agree on are listed (SIGBUS and SIGUSR1 differ between them).
pub(crate) fn signal_name(signal: i32) -> Option<&'static str> {
    Some(match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return None,
    })
}

/// How a process ended, reduced to the facts that decide the wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Termination {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl Termination {
    pub(crate) fn of(status: ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Self {
            code: status.code(),
            signal,
        }
    }

    /// The signal that ended the process. A shell, a container runtime or a
    /// supervisor reports a signal death as exit code `128 + signal`, so 137
    /// is SIGKILL either way. Windows has no signals; its codes stay codes.
    pub(crate) fn fatal_signal(&self) -> Option<i32> {
        if self.signal.is_some() {
            return self.signal;
        }
        #[cfg(unix)]
        if let Some(code) = self.code {
            let signal = code - 128;
            if code > 128 && signal_name(signal).is_some() {
                return Some(signal);
            }
        }
        None
    }

    /// The status half of a crash message. `oom_kills` is the cgroup's
    /// `oom_kill` counter at spawn and at exit when it could be read.
    pub(crate) fn explain(&self, oom_kills: Option<(u64, u64)>) -> String {
        let Some(signal) = self.fatal_signal() else {
            return match self.code {
                Some(code) => format!("exit code {code}"),
                None => "unknown exit".into(),
            };
        };
        let how = match (self.signal, self.code) {
            (None, Some(code)) => format!("exit code {code}, "),
            _ => String::new(),
        };
        let name = signal_name(signal).map_or_else(
            || format!("signal {signal}"),
            |name| format!("{name} (signal {signal})"),
        );
        if signal != 9 {
            return format!("{how}killed by {name}");
        }
        match oom_kills {
            Some((before, after)) if after > before => format!(
                "{how}killed by {name}: out of memory, the kernel's OOM killer fired (oom_kill {before} -> {after})"
            ),
            _ => format!(
                "{how}killed by {name}, likely out of memory, from the system's OOM killer or a memory limit"
            ),
        }
    }
}

/// The cgroup a child runs in and its OOM-kill count when it started. Linux
/// with cgroup v2 only; everywhere else there is nothing to read and
/// `start` returns `None`.
pub(crate) struct OomWatch {
    events: PathBuf,
    before: u64,
}

impl OomWatch {
    /// Read right after spawn, while `/proc/<pid>` exists: it is gone once
    /// the child has been reaped, and the child may sit in a different scope
    /// than the engine (a systemd `MemoryMax` applies to the unit's cgroup).
    pub(crate) fn start(pid: u32) -> Option<Self> {
        let raw = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        let path = cgroup_v2_path(&raw)?;
        let events = PathBuf::from("/sys/fs/cgroup")
            .join(path.trim_start_matches('/'))
            .join("memory.events");
        let before = oom_kill_count(&std::fs::read_to_string(&events).ok()?)?;
        Some(Self { events, before })
    }

    /// `(at spawn, now)`; `None` when the counter can no longer be read.
    pub(crate) fn kills(&self) -> Option<(u64, u64)> {
        let now = oom_kill_count(&std::fs::read_to_string(&self.events).ok()?)?;
        Some((self.before, now))
    }
}

/// The path from a cgroup v2 `/proc/<pid>/cgroup` (a single `0::<path>`
/// line). A v1 or hybrid host lists one line per controller and has no one
/// path to read, so those give `None` rather than a wrong answer.
fn cgroup_v2_path(proc_cgroup: &str) -> Option<&str> {
    let mut lines = proc_cgroup.lines();
    let line = lines.next()?;
    if lines.next().is_some() {
        return None;
    }
    line.strip_prefix("0::").map(str::trim)
}

/// The `oom_kill` counter of a cgroup's `memory.events`.
fn oom_kill_count(events: &str) -> Option<u64> {
    events.lines().find_map(|line| {
        let (key, value) = line.split_once(' ')?;
        if key == "oom_kill" {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn killed_by(signal: i32) -> Termination {
        Termination {
            code: None,
            signal: Some(signal),
        }
    }

    #[test]
    fn signal_names_cover_the_common_fatal_signals() {
        for (signal, name) in [
            (1, "SIGHUP"),
            (6, "SIGABRT"),
            (9, "SIGKILL"),
            (11, "SIGSEGV"),
            (15, "SIGTERM"),
        ] {
            assert_eq!(signal_name(signal), Some(name));
        }
        assert_eq!(signal_name(0), None);
        assert_eq!(signal_name(64), None);
    }

    #[test]
    fn the_memory_events_counter_is_read_and_unknown_keys_are_ignored() {
        let events = "low 0\nhigh 0\nmax 3\noom 1\noom_kill 4\noom_group_kill 0\n";
        assert_eq!(oom_kill_count(events), Some(4));
        assert_eq!(oom_kill_count("low 0\nhigh 0\n"), None);
        assert_eq!(oom_kill_count("oom_kill many\n"), None);
    }

    #[test]
    fn only_a_single_line_cgroup_v2_path_is_trusted() {
        assert_eq!(
            cgroup_v2_path("0::/system.slice/zeron.service\n"),
            Some("/system.slice/zeron.service")
        );
        assert_eq!(
            cgroup_v2_path("12:memory:/user.slice\n0::/user.slice\n"),
            None
        );
        assert_eq!(cgroup_v2_path("1:name=systemd:/x\n"), None);
        assert_eq!(cgroup_v2_path(""), None);
    }

    #[test]
    fn a_kill_is_blamed_on_memory_only_with_the_qualifier_or_the_evidence() {
        let proven = killed_by(9).explain(Some((3, 4)));
        assert!(
            proven.contains("SIGKILL") && proven.contains("signal 9"),
            "{proven}"
        );
        assert!(
            proven.contains("out of memory") && proven.contains("3 -> 4"),
            "{proven}"
        );

        let unproven = killed_by(9).explain(None);
        assert!(
            unproven.contains("killed by SIGKILL (signal 9)"),
            "{unproven}"
        );
        assert!(unproven.contains("likely out of memory"), "{unproven}");
        assert!(!unproven.contains("->"), "{unproven}");

        // A counter that did not move is no evidence either way.
        assert!(
            killed_by(9)
                .explain(Some((4, 4)))
                .contains("likely out of memory")
        );

        let crash = killed_by(11).explain(Some((0, 5)));
        assert!(
            crash.contains("SIGSEGV") && !crash.contains("memory"),
            "{crash}"
        );
        assert_eq!(killed_by(60).explain(None), "killed by signal 60");
    }

    #[test]
    fn a_plain_exit_code_stays_a_code() {
        let exit = Termination {
            code: Some(23),
            signal: None,
        };
        assert_eq!(exit.explain(None), "exit code 23");
        let unknown = Termination {
            code: None,
            signal: None,
        };
        assert_eq!(unknown.explain(None), "unknown exit");
    }

    #[cfg(unix)]
    #[test]
    fn exit_code_137_reads_as_sigkill() {
        let exit = Termination {
            code: Some(137),
            signal: None,
        };
        assert_eq!(exit.fatal_signal(), Some(9));
        let text = exit.explain(None);
        assert!(
            text.contains("exit code 137") && text.contains("SIGKILL"),
            "{text}"
        );
        // 255 is no signal at all.
        assert_eq!(
            Termination {
                code: Some(255),
                signal: None
            }
            .fatal_signal(),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_real_exit_status_is_read_for_its_signal() {
        use std::os::unix::process::ExitStatusExt;
        let killed = Termination::of(ExitStatus::from_raw(9));
        assert_eq!(killed, killed_by(9));
        let exited = Termination::of(ExitStatus::from_raw(23 << 8));
        assert_eq!(exited.code, Some(23));
        assert_eq!(exited.signal, None);
    }
}
