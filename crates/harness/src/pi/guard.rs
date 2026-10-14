//! The delete guard: one bundled Pi extension (`guard.mjs`) loaded into every
//! real Pi run, with or without the MCP bridge, and handed to the user's own
//! child launchers through env. A guard against mistakes, not a sandbox.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{HarnessError, process::Command};

const SOURCE: &str = include_str!("guard.mjs");
static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

/// What a spawn tells the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteGuard {
    /// The Settings toggle at spawn. A running Pi follows `state_file`.
    pub enabled: bool,
    /// `ZERON_BLOCK_DELETES=1` on the engine: on, whatever the file says.
    pub forced: bool,
    /// `harness-prefs.json`, re-read by the extension on each tool call.
    pub state_file: Option<PathBuf>,
    /// Where the extension is installed. Not temp: Linux read-only
    /// subagents run with temp hidden.
    pub dir: PathBuf,
}

/// Read on every spawn, so a toggle reaches the next run.
pub type DeleteGuardSetting = Arc<dyn Fn() -> DeleteGuard + Send + Sync>;

pub(super) fn configure(cmd: &mut Command, guard: &DeleteGuard) -> Result<(), HarnessError> {
    // Fail closed: a run started without its guard is never covered, even
    // once the toggle turns on.
    let path = install(&guard.dir).map_err(|error| {
        HarnessError::Protocol(format!(
            "the delete guard could not be installed in {}: {error}",
            guard.dir.display()
        ))
    })?;
    cmd.arg("--extension").arg(&path);
    cmd.env("ZERON_DELETE_GUARD_EXT", &path)
        .env("ZERON_DELETE_GUARD", if guard.enabled { "1" } else { "0" });
    match &guard.state_file {
        Some(file) => cmd.env("ZERON_DELETE_GUARD_FILE", file),
        None => cmd.env_remove("ZERON_DELETE_GUARD_FILE"),
    };
    // Never inherit a stale value.
    if guard.forced {
        cmd.env("ZERON_DELETE_GUARD_FORCE", "1");
    } else {
        cmd.env_remove("ZERON_DELETE_GUARD_FORCE");
    }
    Ok(())
}

/// FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn current(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|bytes| bytes == SOURCE.as_bytes())
}

/// Content-addressed and never pruned: a long-lived Pi from before an
/// upgrade still hands the old path to its children.
fn install(dir: &Path) -> std::io::Result<PathBuf> {
    let path = dir.join(format!("zeron-delete-guard-{:016x}.mjs", fnv1a(SOURCE)));
    if current(&path) {
        return Ok(path);
    }
    std::fs::create_dir_all(dir)?;
    // A unique temporary per call: two spawns in one engine must not
    // truncate each other's file before it is renamed into place.
    let tmp = dir.join(format!(
        ".zeron-delete-guard-{}-{}.tmp",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, SOURCE)?;
    let renamed = std::fs::rename(&tmp, &path);
    let _ = std::fs::remove_file(&tmp);
    match renamed {
        Ok(()) => Ok(path),
        // A racing spawn won (a Windows rename-over fails while the file is
        // open).
        Err(_) if current(&path) => Ok(path),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_content_addressed_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = install(dir.path()).unwrap();
        assert!(current(&path));
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("zeron-delete-guard-") && name.ends_with(".mjs"));
        assert_eq!(install(dir.path()).unwrap(), path);
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporaries are left behind");
    }

    #[test]
    fn concurrent_installs_agree() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.path().to_path_buf();
                std::thread::spawn(move || install(&dir).unwrap())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert!(paths.windows(2).all(|pair| pair[0] == pair[1]));
        assert!(current(&paths[0]));
    }

    #[test]
    fn a_truncated_install_is_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = install(dir.path()).unwrap();
        std::fs::write(&path, "// cut").unwrap();
        assert_eq!(install(dir.path()).unwrap(), path);
        assert!(current(&path));
    }

    #[test]
    fn the_source_carries_the_tool_call_hook() {
        assert!(SOURCE.contains("pi.on(\"tool_call\""));
        assert!(SOURCE.contains("blockDeletes"));
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
    }
}
