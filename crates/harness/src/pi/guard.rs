//! The delete guard: one bundled Pi extension (`guard.mjs`) loaded into every
//! real Pi run, with or without the MCP bridge. Every Pi that run starts
//! (fleet subagents, loop workers) gets it too: the extension adds a node
//! preload (`guard-preload.mjs`) to the flags Pi's child launchers pass on,
//! and the preload hands each child Pi the extension. Its path is also in
//! env for launchers that pass extensions themselves. A guard against
//! mistakes, not a sandbox.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{HarnessError, process::Command};

const SOURCE: &str = include_str!("guard.mjs");
const PRELOAD: &str = include_str!("guard-preload.mjs");
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
    let installed = install(&guard.dir)
        .and_then(|path| install_preload(&guard.dir).map(|preload| (path, preload)));
    let (path, preload) = installed.map_err(|error| {
        HarnessError::Protocol(format!(
            "the delete guard could not be installed in {}: {error}",
            guard.dir.display()
        ))
    })?;
    cmd.arg("--extension").arg(&path);
    cmd.env("ZERON_DELETE_GUARD_EXT", &path)
        .env("ZERON_DELETE_GUARD_PRELOAD", &preload)
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
    holds(path, SOURCE)
}

fn holds(path: &Path, source: &str) -> bool {
    std::fs::read(path).is_ok_and(|bytes| bytes == source.as_bytes())
}

fn install(dir: &Path) -> std::io::Result<PathBuf> {
    install_file(dir, "zeron-delete-guard", SOURCE)
}

fn install_preload(dir: &Path) -> std::io::Result<PathBuf> {
    install_file(dir, "zeron-delete-guard-preload", PRELOAD)
}

/// Content-addressed and never pruned: a long-lived Pi from before an
/// upgrade still hands the old path to its children.
fn install_file(dir: &Path, stem: &str, source: &str) -> std::io::Result<PathBuf> {
    let path = dir.join(format!("{stem}-{:016x}.mjs", fnv1a(source)));
    if holds(&path, source) {
        return Ok(path);
    }
    std::fs::create_dir_all(dir)?;
    // A unique temporary per call: two spawns in one engine must not
    // truncate each other's file before it is renamed into place.
    let tmp = dir.join(format!(
        ".{stem}-{}-{}.tmp",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, source)?;
    let renamed = std::fs::rename(&tmp, &path);
    let _ = std::fs::remove_file(&tmp);
    match renamed {
        Ok(()) => Ok(path),
        // A racing spawn won (a Windows rename-over fails while the file is
        // open).
        Err(_) if holds(&path, source) => Ok(path),
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
    fn the_preload_installs_beside_the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let guard = install(dir.path()).unwrap();
        let preload = install_preload(dir.path()).unwrap();
        assert_ne!(guard, preload);
        assert!(holds(&preload, PRELOAD));
        let name = preload.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("zeron-delete-guard-preload-"));
        assert_eq!(install_preload(dir.path()).unwrap(), preload);
        assert!(SOURCE.contains("inheritIntoChildren();"));
        assert!(PRELOAD.contains("ZERON_DELETE_GUARD_PI"));
    }

    #[test]
    fn the_source_carries_the_tool_call_hook() {
        assert!(SOURCE.contains("pi.on(\"tool_call\""));
        assert!(SOURCE.contains("blockDeletes"));
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
    }
}
