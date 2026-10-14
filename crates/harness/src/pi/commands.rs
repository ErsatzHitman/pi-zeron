//! Pi's slash commands per workspace, so a `/` menu never starts a Pi process.
//! A chat's own Pi lists its commands when it starts, and that list is what
//! the menu shows (kept in a file beside the session map, so it is there
//! after a restart too). A workspace no chat has run in yet borrows the
//! latest list from the same Pi: extensions are global, and its first run
//! corrects the rest. Only a Pi with no list anywhere is asked by a
//! throwaway process, once.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use zeron_proto::SlashCommand;

const FILE_VERSION: u32 = 3;

/// The same workspace under another Pi executable can list other commands.
pub(super) fn key(executable: &Path, cwd: &Path) -> String {
    format!("{}\n{}", executable.display(), cwd.display())
}

fn executable_of(key: &str) -> &str {
    key.split_once('\n')
        .map_or(key, |(executable, _)| executable)
}

#[derive(Clone, Default)]
pub(super) struct Cache {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    loaded: bool,
    saved: Saved,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    version: u32,
    workspaces: HashMap<String, Vec<SlashCommand>>,
    /// The workspace stored last, per executable.
    #[serde(default)]
    latest: HashMap<String, String>,
}

impl Cache {
    /// `key`'s list, else the latest list from the same executable.
    pub(super) fn lookup(&self, file: &Path, key: &str) -> Option<Vec<SlashCommand>> {
        let mut inner = self.lock();
        if !inner.loaded {
            inner.loaded = true;
            if let Some(saved) = read(file) {
                inner.saved = saved;
            }
        }
        let saved = &inner.saved;
        saved
            .workspaces
            .get(key)
            .or_else(|| {
                let latest = saved.latest.get(executable_of(key))?;
                saved.workspaces.get(latest)
            })
            .cloned()
    }

    pub(super) fn store(&self, file: &Path, key: &str, commands: Vec<SlashCommand>) {
        let snapshot = {
            let mut inner = self.lock();
            inner.loaded = true;
            let saved = &mut inner.saved;
            let unchanged = saved.workspaces.get(key) == Some(&commands)
                && saved.latest.get(executable_of(key)).map(String::as_str) == Some(key);
            if unchanged {
                return;
            }
            saved.version = FILE_VERSION;
            saved.workspaces.insert(key.to_owned(), commands);
            saved
                .latest
                .insert(executable_of(key).to_owned(), key.to_owned());
            serde_json::to_vec(&*saved).ok()
        };
        if let Some(bytes) = snapshot {
            write(file, &bytes);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn read(file: &Path) -> Option<Saved> {
    let bytes = std::fs::read(file).ok()?;
    serde_json::from_slice::<Saved>(&bytes)
        .ok()
        .filter(|saved| saved.version == FILE_VERSION)
}

/// Best effort: the file only saves a wait after a restart.
fn write(file: &Path, bytes: &[u8]) {
    let Some(dir) = file.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let temp: PathBuf = dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    if std::fs::write(&temp, bytes).is_err() || std::fs::rename(&temp, file).is_err() {
        let _ = std::fs::remove_file(&temp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(name: &str) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            description: String::new(),
            input_hint: None,
            arguments: vec![],
        }
    }

    #[test]
    fn a_workspace_gets_its_own_list_else_the_latest_from_the_same_pi() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("commands.json");
        let pi = Path::new("/bin/pi");
        let cache = Cache::default();
        assert!(cache.lookup(&file, &key(pi, Path::new("/a"))).is_none());

        cache.store(&file, &key(pi, Path::new("/a")), vec![command("btw")]);
        cache.store(&file, &key(pi, Path::new("/b")), vec![command("usage")]);
        assert_eq!(
            cache.lookup(&file, &key(pi, Path::new("/a"))),
            Some(vec![command("btw")])
        );
        assert_eq!(
            cache.lookup(&file, &key(pi, Path::new("/new"))),
            Some(vec![command("usage")]),
            "an unseen workspace borrows the latest list"
        );
        assert!(
            cache
                .lookup(&file, &key(Path::new("/other/pi"), Path::new("/a")))
                .is_none(),
            "never another executable's list"
        );
    }

    #[test]
    fn lists_survive_a_restart_and_a_bad_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index").join("commands.json");
        let k = key(Path::new("/bin/pi"), Path::new("/a"));
        Cache::default().store(&file, &k, vec![command("usage")]);
        assert_eq!(
            Cache::default().lookup(&file, &k),
            Some(vec![command("usage")])
        );

        std::fs::write(&file, b"{not json").unwrap();
        assert!(Cache::default().lookup(&file, &k).is_none());
    }
}
