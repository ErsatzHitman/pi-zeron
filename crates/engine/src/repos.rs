//! Repos — this device's git repositories, branches, worktrees, and the folder
//! browser (feature-inventory §3.5; port of zeron's `repos.ts` + `folder-lister.ts`).
//!
//! Repos are device-local (paths differ per machine), so the known set is a plain
//! JSON list (`{data_dir}/repos.json`) — no sync. Existing repos can live anywhere
//! the user points us; cloned/created ones land in `{data_dir}/repos`. Worktrees are
//! created under `~/.zeron/worktrees/<repoName>/<worktreeName>` (NOT the data
//! dir — worktrees are user-facing working checkouts), with an auto-generated name +
//! matching `zeron/<name>` branch. `ZERON_WORKTREES_DIR` overrides the root.
//!
//! All git access is via subprocess (`tokio::process`) — never libgit2.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::{StreamExt, stream};
use sha2::{Digest, Sha256};

use zeron_proto::{
    DriveEntry, FileSearchMatch, FolderEntry, FolderListing, GitHistoryCommit,
    GitHistoryComparison, GitHistoryPage, GitHistoryRef, GitHistoryRefKind, HomeFolderMatch,
    HomeFolderSearchResult, MergeConflictCheck, MergePreview, MergePreviewDirty, MergePreviewFile,
    Repo, RepoRef, Worktree,
};

use crate::EngineError;
use crate::WorkspaceSearch;
use crate::workspace_search::{NameKinds, normalize_name_query};

/// Existence probe timeout for user-chosen / remembered paths, which can point at
/// dead network mounts where a bare `stat` hangs for minutes.
const PATH_EXISTS_TIMEOUT: Duration = Duration::from_secs(2);
/// Hard wall-clock ceiling for a folder listing (the walk runs in a disposable
/// blocking task; on expiry the caller unblocks and the task is abandoned).
const FOLDER_LIST_TIMEOUT: Duration = Duration::from_secs(6);
/// Cap on returned folder entries (bounds response size).
const FOLDER_LIST_MAX_ENTRIES: usize = 500;
/// Cap on returned drives (a machine with more mounts than this is a server
/// farm, not a laptop picking a project folder).
const DRIVE_LIST_MAX_ENTRIES: usize = 50;
/// File mentions should remain responsive even in very large checkouts.
const FILE_SEARCH_MAX_RESULTS: usize = 8;
/// A dead network mount must not leave the composer search spinning forever.
const FILE_SEARCH_TIMEOUT: Duration = Duration::from_secs(6);
/// Folders `SearchHomeFolders` returns by default, and at most.
pub const HOME_FOLDER_RESULTS: usize = 20;
pub const HOME_FOLDER_MAX_RESULTS: usize = 50;
/// Candidates taken from the index before re-ranking by name; more for a
/// `/` query, which searches only its last segment.
const HOME_FOLDER_CANDIDATES: usize = 300;
const HOME_FOLDER_PATH_CANDIDATES: usize = 2_000;
const GITHUB_AVATAR_TIMEOUT: Duration = Duration::from_secs(6);
/// Stable prefix of the error `delete_worktree` returns when it refuses to
/// discard unmerged commits or uncommitted changes (clients match on it).
pub use zeron_rpc::WORKTREE_UNSAFE_PREFIX;
pub const GIT_HISTORY_DEFAULT_LIMIT: usize = 100;
pub const GIT_HISTORY_MAX_LIMIT: usize = 200;

const ADJECTIVES: &[&str] = &[
    "swift", "calm", "bright", "bold", "keen", "brave", "clever", "lucky", "quiet", "warm", "cool",
    "sharp", "gentle", "vivid", "amber", "cobalt",
];
const NOUNS: &[&str] = &[
    "otter", "harbor", "falcon", "cedar", "meadow", "zeron", "delta", "ember", "lynx", "maple",
    "onyx", "quartz", "raven", "summit", "willow", "aspen",
];

/// Canonical identity shared by every chat operating in this exact worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutIdentity {
    /// `sha256(deviceId ‖ NUL ‖ canonical git dir)` — device-scoped, path-stable.
    pub id: String,
    /// Canonical worktree root (`rev-parse --show-toplevel`, symlinks resolved).
    pub root: PathBuf,
    /// Canonical git dir (worktree-specific for linked worktrees).
    pub git_dir: PathBuf,
}

/// Best-effort home directory (the `ListFolders` default and worktree root base).
pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `~` / `~/…` → this host's home directory. Anything else passes through.
/// An unavailable home is an error so `~` never silently becomes `/`.
pub(crate) fn expand_home(cwd: &str) -> Result<String, &'static str> {
    match cwd.strip_prefix('~') {
        Some("") => Ok(session_home_dir()?.to_string_lossy().into_owned()),
        Some(rest) if rest.starts_with('/') => Ok(session_home_dir()?
            .join(&rest[1..])
            .to_string_lossy()
            .into_owned()),
        _ => Ok(cwd.to_string()),
    }
}

/// A chat cwd on this host: [`expand_home`], or a projectless chat's scratch
/// folder under `scratch_root`, created on first use — createChat only names
/// it, possibly on another device.
pub(crate) fn resolve_chat_cwd(cwd: &str, scratch_root: &Path) -> Result<String, String> {
    if !zeron_proto::projectless::is_scratch_cwd(cwd) {
        return expand_home(cwd).map_err(str::to_string);
    }
    let name = cwd.trim().rsplit('/').next().unwrap_or_default();
    let folder = scratch_root.join(name);
    std::fs::create_dir_all(&folder).map_err(|error| {
        format!(
            "could not create the chat's folder {}: {error}",
            folder.display()
        )
    })?;
    Ok(folder.to_string_lossy().into_owned())
}

/// Resolve the host user's home for a session that explicitly uses `~`.
/// An absent home must not silently turn a projectless session into `/`.
pub(crate) fn session_home_dir() -> Result<PathBuf, &'static str> {
    session_home_dir_with(|name| std::env::var_os(name))
}

fn session_home_dir_with(
    mut env: impl FnMut(&str) -> Option<std::ffi::OsString>,
) -> Result<PathBuf, &'static str> {
    #[cfg(windows)]
    let names = ["USERPROFILE", "HOME"];
    #[cfg(not(windows))]
    let names = ["HOME", "USERPROFILE"];
    for name in names {
        let Some(value) = env(name).filter(|value| !value.is_empty()) else {
            continue;
        };
        let path = PathBuf::from(value);
        if path.is_absolute() && path.is_dir() && path.to_str().is_some() {
            return Ok(path);
        }
    }
    Err("User home directory unavailable on this device")
}

/// Where new worktrees live. Deliberately NOT under the backend data dir —
/// worktrees are user-facing working checkouts. `ZERON_WORKTREES_DIR` overrides
/// (test isolation); empty reads as unset.
fn default_worktrees_root() -> PathBuf {
    std::env::var_os("ZERON_WORKTREES_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".zeron").join("worktrees"))
}

struct ReposInner {
    data_dir: PathBuf,
    device_id: String,
    worktrees_root: PathBuf,
    http: reqwest::Client,
    github_avatars: std::sync::Mutex<HashMap<String, String>>,
    github_avatar_pages: std::sync::Mutex<HashSet<String>>,
    /// The engine's one workspace search index set (`@`, file tree, cmd+K).
    workspace_search: WorkspaceSearch,
    /// HEAD commit → its trunk's root commit. History behind a commit never
    /// changes, so the full first-parent walk runs once per HEAD, not on
    /// every spaces repair pass.
    trunk_roots: std::sync::Mutex<HashMap<String, String>>,
}

#[derive(Clone)]
pub struct Repos {
    inner: std::sync::Arc<ReposInner>,
}

impl Repos {
    pub(crate) fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    /// Where projectless chats' folders live on this host: the engine data
    /// dir's `scratch` (`~/.zeron/scratch` for the default data dir), as
    /// t3code keeps its under `~/.t3/scratch`.
    pub(crate) fn scratch_root(&self) -> PathBuf {
        self.inner.data_dir.join("scratch")
    }

    pub fn workspace_search(&self) -> &WorkspaceSearch {
        &self.inner.workspace_search
    }

    /// `data_dir` holds `repos.json` + cloned/created repos; the worktree root
    /// comes from `$ZERON_WORKTREES_DIR` or `~/.zeron/worktrees`.
    pub fn new(data_dir: &Path, device_id: &str) -> Self {
        Self::with_worktrees_root(data_dir, device_id, default_worktrees_root())
    }

    /// Explicit worktree root (tests).
    pub fn with_worktrees_root(data_dir: &Path, device_id: &str, worktrees_root: PathBuf) -> Self {
        Self {
            inner: std::sync::Arc::new(ReposInner {
                data_dir: data_dir.to_path_buf(),
                device_id: device_id.to_string(),
                worktrees_root,
                http: reqwest::Client::builder()
                    .timeout(GITHUB_AVATAR_TIMEOUT)
                    .user_agent("Comet-Git-History")
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new()),
                github_avatars: std::sync::Mutex::new(HashMap::new()),
                trunk_roots: std::sync::Mutex::new(HashMap::new()),
                github_avatar_pages: std::sync::Mutex::new(HashSet::new()),
                workspace_search: WorkspaceSearch::new(),
            }),
        }
    }

    // ── registry (repos.json) ───────────────────────────────────────────────

    fn registry_path(&self) -> PathBuf {
        self.inner.data_dir.join("repos.json")
    }

    fn load_paths(&self) -> Vec<String> {
        std::fs::read_to_string(self.registry_path())
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default()
    }

    fn save_paths(&self, paths: &[String]) -> Result<(), EngineError> {
        let mut seen = HashSet::new();
        let deduped: Vec<&String> = paths.iter().filter(|p| seen.insert(p.as_str())).collect();
        let json = serde_json::to_string_pretty(&deduped)
            .map_err(|e| EngineError::Other(format!("repos registry serialize: {e}")))?;
        std::fs::create_dir_all(&self.inner.data_dir)?;
        std::fs::write(self.registry_path(), json)?;
        Ok(())
    }

    fn register(&self, path: &str) -> Result<(), EngineError> {
        let mut paths = self.load_paths();
        paths.push(path.to_string());
        self.save_paths(&paths)
    }

    // ── git plumbing ────────────────────────────────────────────────────────

    /// Run `git <args>` (optionally under `cwd`), returning trimmed stdout.
    async fn git(&self, args: &[&str], cwd: Option<&Path>) -> Result<String, EngineError> {
        let mut cmd = tokio::process::Command::new("git");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.as_std_mut().creation_flags(0x08000000);
        }
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(std::process::Stdio::null());
        let output = cmd
            .output()
            .await
            .map_err(|e| EngineError::Other(format!("git spawn failed: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            return Err(EngineError::Other(if message.is_empty() {
                format!(
                    "git {} failed ({})",
                    args.first().unwrap_or(&"?"),
                    output.status
                )
            } else {
                format!("git: {message}")
            }));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Like [`Self::git`], for commands that answer "no" with a silent exit
    /// status 1 (`rev-parse --verify --quiet`): `Ok(None)` then, `Err` for
    /// any other failure.
    async fn git_probe(&self, args: &[&str], cwd: Option<&Path>) -> Result<Option<String>, EngineError> {
        let mut cmd = tokio::process::Command::new("git");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.as_std_mut().creation_flags(0x08000000);
        }
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(std::process::Stdio::null());
        let output = cmd
            .output()
            .await
            .map_err(|e| EngineError::Other(format!("git spawn failed: {e}")))?;
        if output.status.success() {
            return Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string()));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(1) && stderr.trim().is_empty() {
            return Ok(None);
        }
        Err(EngineError::Other(format!(
            "git {} failed ({}): {}",
            args.first().unwrap_or(&"?"),
            output.status,
            stderr.trim()
        )))
    }

    /// Like [`Self::git`], for commands whose exit code carries meaning
    /// (`merge-tree` exits 1 on conflicts): `(exit code, untrimmed stdout,
    /// stderr)` for ANY exit, `Err` only when git cannot be spawned.
    async fn git_status(
        &self,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> Result<(i32, String, String), EngineError> {
        let mut cmd = tokio::process::Command::new("git");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.as_std_mut().creation_flags(0x08000000);
        }
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(std::process::Stdio::null());
        let output = cmd
            .output()
            .await
            .map_err(|e| EngineError::Other(format!("git spawn failed: {e}")))?;
        Ok((
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }

    /// The installed git as `(major, minor, version text)`, if it parses.
    async fn git_version(&self) -> Option<(u32, u32, String)> {
        let (code, out, _) = self.git_status(&["--version"], None).await.ok()?;
        if code != 0 {
            return None;
        }
        parse_git_version(&out)
    }

    /// Async existence probe with a timeout: a wedged network mount just reads
    /// as "gone" instead of hanging every caller.
    async fn path_exists(path: &Path) -> bool {
        let path = path.to_path_buf();
        matches!(
            tokio::time::timeout(PATH_EXISTS_TIMEOUT, tokio::fs::metadata(path)).await,
            Ok(Ok(_))
        )
    }

    /// Is `path` inside a git work tree? (Also the SpacesSync git-presence probe.)
    pub async fn is_repo(&self, path: &Path) -> bool {
        matches!(
            self.git(&["rev-parse", "--is-inside-work-tree"], Some(path)).await,
            Ok(out) if out == "true"
        )
    }

    /// The branch currently checked out at a repo/worktree path (`"HEAD"` when detached).
    pub async fn current_branch(&self, path: &Path) -> Result<String, EngineError> {
        let branch = self.git(&["branch", "--show-current"], Some(path)).await?;
        Ok(if branch.is_empty() {
            "HEAD".to_string()
        } else {
            branch
        })
    }

    /// Commit checked out at `path`, or `None` for a repository without an
    /// initial commit.
    pub async fn head_sha(&self, path: &Path) -> Result<Option<String>, EngineError> {
        match self
            .git(&["rev-parse", "--verify", "HEAD^{commit}"], Some(path))
            .await
        {
            Ok(sha) if !sha.is_empty() => Ok(Some(sha)),
            Ok(_) => Ok(None),
            Err(_) if self.is_repo(path).await => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Fetch every configured remote without pruning or integrating anything
    /// into the active branch. This intentionally updates refs only.
    pub async fn fetch_all(&self, repo_path: &Path) -> Result<(), EngineError> {
        self.git(&["fetch", "--all", "--quiet"], Some(repo_path))
            .await
            .map(drop)
    }

    /// The absolute Git `HEAD` file for event-driven external branch reconciliation.
    pub async fn git_head_path(&self, path: &Path) -> Result<PathBuf, EngineError> {
        let git_dir = self
            .git(&["rev-parse", "--absolute-git-dir"], Some(path))
            .await?;
        Ok(PathBuf::from(git_dir).join("HEAD"))
    }

    /// Canonical identity shared by every chat operating in this exact worktree:
    /// `sha256(deviceId ‖ NUL ‖ canonical git dir)`.
    pub async fn checkout_identity(&self, path: &Path) -> Result<CheckoutIdentity, EngineError> {
        let root = self
            .git(&["rev-parse", "--show-toplevel"], Some(path))
            .await?;
        let git_dir = self
            .git(
                &["rev-parse", "--path-format=absolute", "--git-dir"],
                Some(path),
            )
            .await?;
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| PathBuf::from(&root));
        let canonical_git_dir =
            std::fs::canonicalize(&git_dir).unwrap_or_else(|_| PathBuf::from(&git_dir));
        let mut hasher = Sha256::new();
        hasher.update(self.inner.device_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(canonical_git_dir.to_string_lossy().as_bytes());
        let id = hex(&hasher.finalize());
        Ok(CheckoutIdentity {
            id,
            root: canonical_root,
            git_dir: canonical_git_dir,
        })
    }

    /// Identity shared by every clone and worktree of one repository: the
    /// root commit its trunk grew from, `commit:<sha>`. Unlike a remote URL it
    /// survives repository renames and transfers, SSH host aliases and
    /// transport spellings, and it needs no network. Forks share it with
    /// their upstream (the same codebase).
    ///
    /// A history that cannot vouch for its root — a shallow clone, or no
    /// commits yet — falls back to the normalized `origin` remote (else the
    /// first remote) as `host/owner/repo`, then to
    /// `local:sha256(deviceId ‖ NUL ‖ canonical common git dir)`, so the
    /// repository's worktrees on this device still match each other.
    ///
    /// A folder below the repository's top level carries its in-repository
    /// path (`<identity>:apps/web`): sibling folders of one monorepo, or
    /// plain folders inside an enclosing repository, stay separate projects,
    /// while the same subfolder in another clone or worktree still matches.
    ///
    /// Errors are real failures (git missing, a lock, I/O) — never a silent
    /// fallback to a different identity form, which would regroup the
    /// sidebar on a transient hiccup. Callers keep their last value.
    pub async fn repository_identity(&self, path: &Path) -> Result<String, EngineError> {
        let base = match self.trunk_root_commit(path).await? {
            Some(root) => format!("commit:{root}"),
            None => self.remote_or_local_identity(path).await?,
        };
        let prefix = self.git(&["rev-parse", "--show-prefix"], Some(path)).await?;
        let prefix = prefix.trim_end_matches('/');
        Ok(if prefix.is_empty() {
            base
        } else {
            format!("{base}:{prefix}")
        })
    }

    /// The normalized remote, else the device-scoped common-git-dir hash.
    async fn remote_or_local_identity(&self, path: &Path) -> Result<String, EngineError> {
        let remote = match self.git(&["remote", "get-url", "origin"], Some(path)).await {
            Ok(url) => Some(url),
            Err(_) => match self.git(&["remote"], Some(path)).await {
                Ok(names) => match names.lines().next() {
                    Some(name) => self
                        .git(&["remote", "get-url", name.trim()], Some(path))
                        .await
                        .ok(),
                    None => None,
                },
                Err(_) => None,
            },
        };
        if let Some(remote) = remote
            .as_deref()
            .and_then(crate::source_control::parse_git_remote)
        {
            return Ok(
                format!("{}/{}/{}", remote.host, remote.owner, remote.repository)
                    .to_ascii_lowercase(),
            );
        }
        let common_dir = self
            .git(
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                Some(path),
            )
            .await?;
        let canonical =
            std::fs::canonicalize(&common_dir).unwrap_or_else(|_| PathBuf::from(&common_dir));
        let mut hasher = Sha256::new();
        hasher.update(self.inner.device_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(canonical.to_string_lossy().as_bytes());
        Ok(format!("local:{}", hex(&hasher.finalize())))
    }

    /// The root of HEAD's first-parent chain — the trunk's first commit. A
    /// merged-in unrelated history arrives as a second parent, so its roots
    /// never displace this one. `Ok(None)` when the history can't vouch for
    /// a root: a shallow clone (its boundary commits only look parentless,
    /// and differ with the fetch depth) or an unborn HEAD.
    async fn trunk_root_commit(&self, path: &Path) -> Result<Option<String>, EngineError> {
        let shallow = self
            .git(&["rev-parse", "--is-shallow-repository"], Some(path))
            .await?;
        if shallow == "true" {
            return Ok(None);
        }
        let Some(head) = self
            .git_probe(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"], Some(path))
            .await?
        else {
            return Ok(None); // unborn HEAD: no commits yet
        };
        if let Some(root) = self.inner.trunk_roots.lock().ok().and_then(|cache| cache.get(&head).cloned()) {
            return Ok(Some(root));
        }
        let roots = self
            .git(
                &["rev-list", "--first-parent", "--max-parents=0", &head],
                Some(path),
            )
            .await?;
        let Some(root) = roots
            .lines()
            .map(str::trim)
            .find(|sha| !sha.is_empty())
            .map(str::to_owned)
        else {
            return Ok(None);
        };
        if let Ok(mut cache) = self.inner.trunk_roots.lock() {
            if cache.len() >= 4096 {
                cache.clear();
            }
            cache.insert(head, root.clone());
        }
        Ok(Some(root))
    }

    async fn to_repo(&self, path: &Path) -> Result<Repo, EngineError> {
        let branch = self.current_branch(path).await.ok();
        Ok(Repo {
            path: path.to_string_lossy().to_string(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string()),
            default_branch: branch,
        })
    }

    // ── ListRepos / AddRepo / CloneRepo / CreateRepo ────────────────────────

    /// Known repos that still exist, each with its current branch. Never fails:
    /// vanished paths and non-repos are silently dropped.
    pub async fn list(&self) -> Vec<Repo> {
        let mut repos = Vec::new();
        for path in self.load_paths() {
            let path = PathBuf::from(path);
            if !Self::path_exists(&path).await || !self.is_repo(&path).await {
                continue;
            }
            match self.to_repo(&path).await {
                Ok(repo) => repos.push(repo),
                Err(err) => {
                    tracing::debug!(path = %path.display(), error = %err, "repo listing skip")
                }
            }
        }
        repos
    }

    /// Remember an existing repository the user pointed us at.
    pub async fn add(&self, path: &str) -> Result<Repo, EngineError> {
        let abs = absolutize(Path::new(path));
        if !Self::path_exists(&abs).await {
            return Err(EngineError::Other(format!(
                "No such folder: {}",
                abs.display()
            )));
        }
        if !self.is_repo(&abs).await {
            return Err(EngineError::Other(format!(
                "Not a git repository: {}",
                abs.display()
            )));
        }
        self.register(&abs.to_string_lossy())?;
        self.to_repo(&abs).await
    }

    /// `git clone <url>` under `{data_dir}/repos`. (Named `clone_repo` to keep
    /// `Clone::clone` unambiguous on the service handle.)
    pub async fn clone_repo(&self, url: &str) -> Result<Repo, EngineError> {
        let trimmed = url.trim().trim_end_matches('/');
        let name = trimmed
            .trim_end_matches(".git")
            .rsplit(['/', ':'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("repo")
            .to_string();
        let repos_dir = self.inner.data_dir.join("repos");
        let target = repos_dir.join(&name);
        if target.exists() {
            return Err(EngineError::Other(format!(
                "Already exists: {}",
                target.display()
            )));
        }
        std::fs::create_dir_all(&repos_dir)?;
        self.git(&["clone", trimmed, &target.to_string_lossy()], None)
            .await?;
        self.register(&target.to_string_lossy())?;
        self.to_repo(&target).await
    }

    /// `git init -b main` a fresh repository under `{data_dir}/repos`.
    pub async fn create(&self, name: &str) -> Result<Repo, EngineError> {
        let clean: String = name
            .trim()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        if clean.is_empty() || clean.chars().all(|c| c == '-' || c == '.') {
            return Err(EngineError::Other("Invalid repository name".into()));
        }
        let target = self.inner.data_dir.join("repos").join(&clean);
        if target.exists() {
            return Err(EngineError::Other(format!(
                "Already exists: {}",
                target.display()
            )));
        }
        std::fs::create_dir_all(&target)?;
        self.git(&["init", "-b", "main"], Some(&target)).await?;
        self.register(&target.to_string_lossy())?;
        self.to_repo(&target).await
    }

    // ── branches ────────────────────────────────────────────────────────────

    /// All branches (`git branch -a`), local first, deduped against their remote
    /// counterparts, with the repo's default branch first.
    pub async fn branches(&self, repo_path: &Path) -> Result<Vec<String>, EngineError> {
        let out = self
            .git(&["branch", "-a", "--format=%(refname)"], Some(repo_path))
            .await?;
        let mut names: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut push = |name: &str| {
            if !name.is_empty() && name != "HEAD" && seen.insert(name.to_string()) {
                names.push(name.to_string());
            }
        };
        // Locals first, then remote-only branches (stripped of their remote prefix).
        for line in out.lines().map(str::trim) {
            if let Some(local) = line.strip_prefix("refs/heads/") {
                push(local);
            }
        }
        for line in out.lines().map(str::trim) {
            if let Some(remote) = line.strip_prefix("refs/remotes/")
                && let Some((_, name)) = remote.split_once('/')
            {
                push(name);
            }
        }
        // Default branch first: origin/HEAD's target, else the checked-out branch.
        let default = match self
            .git(
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
                Some(repo_path),
            )
            .await
        {
            Ok(short) => short.split_once('/').map(|(_, b)| b.to_string()),
            Err(_) => None,
        };
        let default = match default {
            Some(branch) => Some(branch),
            None => self
                .current_branch(repo_path)
                .await
                .ok()
                .filter(|b| b != "HEAD"),
        };
        if let Some(default) = default
            && let Some(pos) = names.iter().position(|n| *n == default)
        {
            let head = names.remove(pos);
            names.insert(0, head);
        }
        Ok(names)
    }

    /// [`Self::branches`] enriched with checkout state: which branch the MAIN
    /// folder has checked out (`current`) and which branches are materialized
    /// as linked worktrees (`worktree_path`). Feeds the composer's ref picker
    /// and its checkout-kind selector.
    pub async fn refs(&self, repo_path: &Path) -> Result<Vec<RepoRef>, EngineError> {
        let names = self.branches(repo_path).await?;
        let current = self.current_branch(repo_path).await.ok();
        // `git worktree list --porcelain`: stanzas of `worktree <path>` /
        // `HEAD <sha>` / `branch refs/heads/<name>`. The first stanza is the
        // main checkout — excluded (it's `current`, not a linked worktree).
        let mut worktrees: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Ok(out) = self
            .git(&["worktree", "list", "--porcelain"], Some(repo_path))
            .await
        {
            let mut stanza = 0usize;
            let mut path: Option<String> = None;
            for line in out.lines().map(str::trim) {
                if let Some(p) = line.strip_prefix("worktree ") {
                    stanza += 1;
                    // The first stanza is the main checkout, not a linked tree.
                    path = (stanza > 1).then(|| p.to_string());
                } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
                    && let Some(path) = path.take()
                {
                    worktrees.insert(branch.to_string(), path);
                }
            }
        }
        Ok(names
            .into_iter()
            .map(|name| RepoRef {
                current: current.as_deref() == Some(name.as_str()),
                worktree_path: worktrees.get(&name).cloned(),
                name,
            })
            .collect())
    }

    /// Public commit history in topological order. Only user-facing branches,
    /// remotes, and tags seed the walk, so Zeron's internal refs never leak
    /// into the graph or keep otherwise-unreachable checkpoints visible.
    pub async fn history(
        &self,
        repo_path: &Path,
        cursor: usize,
        limit: usize,
    ) -> Result<GitHistoryPage, EngineError> {
        let limit = limit.clamp(1, GIT_HISTORY_MAX_LIMIT);
        let head_sha = self
            .git(&["rev-parse", "--verify", "HEAD^{commit}"], Some(repo_path))
            .await
            .ok()
            .filter(|sha| !sha.is_empty());

        let refs_out = self
            .git(
                &[
                    "for-each-ref",
                    "--format=%(refname)%00%(objectname)%00%(objecttype)%00%(*objectname)%00%(*objecttype)%00%(symref)%00",
                    "refs/heads",
                    "refs/remotes",
                    "refs/tags",
                ],
                Some(repo_path),
            )
            .await?;
        let refs_by_sha = parse_history_refs(&refs_out);

        if head_sha.is_none() && refs_by_sha.is_empty() {
            return Ok(GitHistoryPage {
                commits: Vec::new(),
                branch_tips: Vec::new(),
                head_sha: None,
                next_cursor: None,
                total_count: Some(0),
                head_commit_count: Some(0),
                comparison: None,
            });
        }

        let skip = format!("--skip={cursor}");
        let max_count = format!("--max-count={}", limit + 1);
        let mut log_args = vec![
            "log",
            "--topo-order",
            "--no-color",
            "--no-decorate",
            "--no-show-signature",
            "--no-patch",
            skip.as_str(),
            max_count.as_str(),
            "--format=%H%x00%P%x00%s%x00%an%x00%ae%x00%aI%x00",
        ];
        if head_sha.is_some() {
            log_args.push("HEAD");
        }
        log_args.extend(["--branches", "--remotes", "--tags"]);
        let log = self.git(&log_args, Some(repo_path)).await?;
        let mut commits = parse_history_log(&log, &refs_by_sha);
        let has_next = commits.len() > limit;
        commits.truncate(limit);

        // Branch tips are deliberately independent from history pagination.
        // `--no-walk` resolves every local/remote tip while deduplicating refs
        // that point at the same commit. Include HEAD as a useful anchor for a
        // detached checkout, but do not let tags seed extra overview rows.
        let branch_tips = if cursor == 0 {
            let mut tip_args = vec![
                "log",
                "--no-walk=sorted",
                "--no-color",
                "--no-decorate",
                "--no-show-signature",
                "--no-patch",
                "--format=%H%x00%P%x00%s%x00%an%x00%ae%x00%aI%x00",
            ];
            if head_sha.is_some() {
                tip_args.push("HEAD");
            }
            tip_args.extend(["--branches", "--remotes"]);
            let tips = self.git(&tip_args, Some(repo_path)).await?;
            parse_history_log(&tips, &refs_by_sha)
        } else {
            Vec::new()
        };

        let total_count = if cursor == 0 {
            let mut count_args = vec!["rev-list", "--count"];
            if head_sha.is_some() {
                count_args.push("HEAD");
            }
            count_args.extend(["--branches", "--remotes", "--tags"]);
            self.git(&count_args, Some(repo_path))
                .await
                .ok()
                .and_then(|count| count.parse().ok())
        } else {
            None
        };
        let head_commit_count = if cursor == 0 && head_sha.is_some() {
            self.git(&["rev-list", "--count", "HEAD"], Some(repo_path))
                .await
                .ok()
                .and_then(|count| count.parse().ok())
        } else {
            None
        };
        let comparison = if cursor == 0 && head_sha.is_some() {
            self.history_comparison(repo_path).await
        } else {
            None
        };

        Ok(GitHistoryPage {
            next_cursor: has_next.then_some(cursor + commits.len()),
            commits,
            branch_tips,
            head_sha,
            total_count,
            head_commit_count,
            comparison,
        })
    }

    /// Compare the checked-out branch with the best locally available
    /// integration ref. This deliberately never talks to the network: `Fetch
    /// all` refreshes remote-tracking refs, then the next History load sees
    /// those new counts.
    async fn history_comparison(&self, repo_path: &Path) -> Option<GitHistoryComparison> {
        // Detached checkouts do not have a branch relationship to present.
        self.git(
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            Some(repo_path),
        )
        .await
        .ok()?;

        // An integration remote is more useful than the branch's push/tracking
        // ref: a feature usually tracks origin/feature, whereas the status the
        // user needs in History is its relationship to upstream/main.
        let mut candidates = Vec::new();
        for remote in ["upstream", "origin"] {
            let remote_head = format!("refs/remotes/{remote}/HEAD");
            if let Ok(base) = self
                .git(
                    &["symbolic-ref", "--quiet", "--short", &remote_head],
                    Some(repo_path),
                )
                .await
                && !base.is_empty()
            {
                candidates.push(base);
            }
        }
        // A remote HEAD is not guaranteed to have been configured locally.
        // Conventional default names keep the result useful in that case.
        candidates.extend([
            "upstream/main".to_string(),
            "origin/main".to_string(),
            "upstream/master".to_string(),
            "origin/master".to_string(),
        ]);
        // Fall back to the configured tracking ref only after integration
        // defaults. This still gives sensible data in a single-remote repo.
        if let Ok(base) = self
            .git(
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
                Some(repo_path),
            )
            .await
            && !base.is_empty()
        {
            candidates.push(base);
        }

        let mut seen = HashSet::new();
        for base in candidates
            .into_iter()
            .filter(|base| seen.insert(base.clone()))
        {
            let commit_ref = format!("{base}^{{commit}}");
            if self
                .git(
                    &["rev-parse", "--verify", "--quiet", &commit_ref],
                    Some(repo_path),
                )
                .await
                .is_err()
            {
                continue;
            }
            let range = format!("HEAD...{base}");
            let Ok(counts) = self
                .git(
                    &["rev-list", "--left-right", "--count", &range],
                    Some(repo_path),
                )
                .await
            else {
                continue;
            };
            let mut counts = counts.split_whitespace();
            let (Some(ahead), Some(behind)) = (counts.next(), counts.next()) else {
                continue;
            };
            let (Ok(ahead), Ok(behind)) = (ahead.parse(), behind.parse()) else {
                continue;
            };
            return Some(GitHistoryComparison {
                base,
                ahead,
                behind,
            });
        }
        None
    }

    /// Fuzzy subject / SHA search across the complete public history. Results
    /// stay in `--topo-order`; fuzzy score decides inclusion, never row order,
    /// so the client can keep rendering a meaningful commit graph.
    pub async fn search_history(
        &self,
        repo_path: &Path,
        query: &str,
        cursor: usize,
        limit: usize,
    ) -> Result<GitHistoryPage, EngineError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(GitHistoryPage {
                commits: Vec::new(),
                branch_tips: Vec::new(),
                head_sha: None,
                next_cursor: None,
                total_count: Some(0),
                head_commit_count: None,
                comparison: None,
            });
        }
        let limit = limit.clamp(1, GIT_HISTORY_MAX_LIMIT);
        let head_sha = self
            .git(&["rev-parse", "--verify", "HEAD^{commit}"], Some(repo_path))
            .await
            .ok()
            .filter(|sha| !sha.is_empty());
        let refs_out = self
            .git(
                &[
                    "for-each-ref",
                    "--format=%(refname)%00%(objectname)%00%(objecttype)%00%(*objectname)%00%(*objecttype)%00%(symref)%00",
                    "refs/heads",
                    "refs/remotes",
                    "refs/tags",
                ],
                Some(repo_path),
            )
            .await?;
        let refs_by_sha = parse_history_refs(&refs_out);
        if head_sha.is_none() && refs_by_sha.is_empty() {
            return Ok(GitHistoryPage {
                commits: Vec::new(),
                branch_tips: Vec::new(),
                head_sha: None,
                next_cursor: None,
                total_count: Some(0),
                head_commit_count: None,
                comparison: None,
            });
        }

        let mut log_args = vec![
            "log",
            "--topo-order",
            "--no-color",
            "--no-decorate",
            "--no-show-signature",
            "--no-patch",
            "--format=%H%x00%P%x00%s%x00%an%x00%ae%x00%aI%x00",
        ];
        if head_sha.is_some() {
            log_args.push("HEAD");
        }
        log_args.extend(["--branches", "--remotes", "--tags"]);
        let log = self.git(&log_args, Some(repo_path)).await?;
        let all_commits = parse_history_log(&log, &refs_by_sha);
        let visible: HashSet<String> = all_commits
            .iter()
            .filter(|commit| git_history_matches(query, commit))
            .map(|commit| commit.sha.clone())
            .collect();
        let matches = compact_history_commits(&all_commits, &visible);
        let total_count = matches.len();
        let start = cursor.min(total_count);
        let end = start.saturating_add(limit).min(total_count);
        let commits = matches[start..end].to_vec();

        Ok(GitHistoryPage {
            commits,
            branch_tips: Vec::new(),
            head_sha,
            next_cursor: (end < total_count).then_some(end),
            total_count: Some(total_count),
            head_commit_count: None,
            comparison: None,
        })
    }

    /// Best-effort GitHub profile images for the authors in one history page.
    /// Git itself only stores names and emails, so this resolves the hosting
    /// metadata separately and caches it by both commit and normalized email.
    pub async fn history_avatar_urls(
        &self,
        repo_path: &Path,
        authors: &[(String, String)],
        cursor: usize,
        limit: usize,
    ) -> HashMap<String, String> {
        let Ok(remote) = self
            .git(&["remote", "get-url", "origin"], Some(repo_path))
            .await
        else {
            return HashMap::new();
        };
        let Some((owner, repo)) = parse_github_remote(&remote) else {
            return HashMap::new();
        };
        let repo_key = format!("{owner}/{repo}").to_ascii_lowercase();
        let per_page = limit.clamp(1, 100);
        let page = cursor / per_page + 1;
        let page_key = format!("{repo_key}|{page}|{per_page}");
        let should_fetch = self
            .inner
            .github_avatar_pages
            .lock()
            .map(|mut pages| pages.insert(page_key.clone()))
            .unwrap_or(false);

        if should_fetch {
            let url = format!("https://api.github.com/repos/{owner}/{repo}/commits");
            let mut request = self.inner.http.get(url).query(&[
                ("per_page", per_page.to_string()),
                ("page", page.to_string()),
            ]);
            if let Some(token) = std::env::var("GITHUB_TOKEN")
                .ok()
                .filter(|token| !token.is_empty())
                .or_else(|| {
                    std::env::var("GH_TOKEN")
                        .ok()
                        .filter(|token| !token.is_empty())
                })
            {
                request = request.bearer_auth(token);
            }

            let rows = match request.send().await {
                Ok(response) if response.status().is_success() => {
                    response.json::<Vec<GitHubCommitAvatar>>().await.ok()
                }
                _ => None,
            };
            if let Some(rows) = rows {
                let mut identities_by_url: HashMap<String, Vec<(String, String)>> = HashMap::new();
                for row in rows {
                    let Some(author) = row.author else {
                        continue;
                    };
                    if !author.avatar_url.starts_with("https://") {
                        continue;
                    }
                    let avatar_url = if author.avatar_url.contains('?') {
                        format!("{}&s=40", author.avatar_url)
                    } else {
                        format!("{}?s=40", author.avatar_url)
                    };
                    identities_by_url.entry(avatar_url).or_default().push((
                        row.sha.to_ascii_lowercase(),
                        row.commit.author.email.trim().to_ascii_lowercase(),
                    ));
                }
                let downloads = stream::iter(identities_by_url.into_iter().map(
                    |(url, identities)| async move {
                        self.cache_github_avatar(&url)
                            .await
                            .map(|path| (identities, path))
                    },
                ))
                .buffer_unordered(8)
                .filter_map(|download| async move { download })
                .collect::<Vec<_>>()
                .await;
                if let Ok(mut cache) = self.inner.github_avatars.lock() {
                    for (identities, path) in downloads {
                        for (sha, email) in identities {
                            cache.insert(format!("{repo_key}|sha|{sha}"), path.clone());
                            if !email.is_empty() {
                                cache.insert(format!("{repo_key}|email|{email}"), path.clone());
                            }
                        }
                    }
                }
            } else if let Ok(mut pages) = self.inner.github_avatar_pages.lock() {
                // A transient network/auth failure may be retried on refresh.
                pages.remove(&page_key);
            }
        }

        let Ok(cache) = self.inner.github_avatars.lock() else {
            return HashMap::new();
        };
        authors
            .iter()
            .filter_map(|(sha, email)| {
                let avatar = cache
                    .get(&format!("{repo_key}|sha|{}", sha.to_ascii_lowercase()))
                    .or_else(|| {
                        cache.get(&format!(
                            "{repo_key}|email|{}",
                            email.trim().to_ascii_lowercase()
                        ))
                    })?;
                Some((email.trim().to_ascii_lowercase(), avatar.clone()))
            })
            .collect()
    }

    async fn cache_github_avatar(&self, url: &str) -> Option<String> {
        const MAX_AVATAR_BYTES: u64 = 2 * 1024 * 1024;

        let digest = Sha256::digest(url.as_bytes());
        let cache_dir = self.inner.data_dir.join("cache").join("git-avatars");
        let path = cache_dir.join(format!("{}.img", hex(&digest[..16])));
        if tokio::fs::metadata(&path)
            .await
            .ok()
            .is_some_and(|metadata| metadata.len() > 0)
        {
            return Some(path.to_string_lossy().into_owned());
        }
        tokio::fs::create_dir_all(&cache_dir).await.ok()?;
        let response = self.inner.http.get(url).send().await.ok()?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|size| size > MAX_AVATAR_BYTES)
        {
            return None;
        }
        let bytes = response.bytes().await.ok()?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_AVATAR_BYTES {
            return None;
        }
        let temporary = cache_dir.join(format!(
            ".{}.{}.tmp",
            hex(&digest[..8]),
            uuid::Uuid::new_v4()
        ));
        tokio::fs::write(&temporary, &bytes).await.ok()?;
        if tokio::fs::rename(&temporary, &path).await.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
            if !path.is_file() {
                return None;
            }
        }
        Some(path.to_string_lossy().into_owned())
    }

    /// Whether `candidate` is the repository root or one of its linked
    /// worktrees. Filesystem resolution happens on a disposable thread because
    /// user-selected paths may be dead mounts.
    pub async fn workspace_checkout(&self, repo_path: &Path, candidate: &Path) -> Option<PathBuf> {
        let repo_path = repo_path.to_path_buf();
        let candidate = candidate.to_path_buf();
        let worktrees: Vec<_> = self
            .refs(&repo_path)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| row.worktree_path.map(PathBuf::from))
            .collect();
        disposable_worker("checkout-auth", move || {
            let candidate = std::fs::canonicalize(candidate).ok()?;
            std::iter::once(repo_path)
                .chain(worktrees)
                .filter_map(|path| std::fs::canonicalize(path).ok())
                .any(|path| path == candidate)
                .then_some(candidate)
        })
        .await
        .flatten()
    }

    /// Switch the checkout at `cwd` (a main folder OR a linked worktree) to
    /// `ref_name` — the t3code `switchRef` port: an existing local branch is
    /// checked out directly; a remote-only branch gets a local tracking
    /// branch (`checkout --track origin/<ref>`). A dirty tree or a branch
    /// already checked out in another worktree fails with git's own message.
    /// Returns the resulting current branch.
    pub async fn switch_ref(&self, cwd: &Path, ref_name: &str) -> Result<String, EngineError> {
        let local = self
            .git(
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{ref_name}"),
                ],
                Some(cwd),
            )
            .await
            .is_ok();
        if local {
            self.git(&["checkout", ref_name], Some(cwd)).await?;
        } else {
            let remote = format!("origin/{ref_name}");
            let has_remote = self
                .git(
                    &[
                        "show-ref",
                        "--verify",
                        "--quiet",
                        &format!("refs/remotes/{remote}"),
                    ],
                    Some(cwd),
                )
                .await
                .is_ok();
            if has_remote {
                self.git(&["checkout", "--track", &remote], Some(cwd))
                    .await?;
            } else {
                // Unknown ref: let git produce the authoritative error.
                self.git(&["checkout", ref_name], Some(cwd)).await?;
            }
        }
        let out = self.git(&["branch", "--show-current"], Some(cwd)).await?;
        Ok(out.trim().to_string())
    }

    // ── worktrees ───────────────────────────────────────────────────────────

    /// `git worktree add` an isolated checkout under
    /// `{worktrees_root}/<repoName>/<generatedName>`, on a fresh `zeron/<name>`
    /// branch off `branch`.
    pub async fn create_worktree(
        &self,
        repo_path: &Path,
        branch: &str,
    ) -> Result<Worktree, EngineError> {
        let repo_name = repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        let base = self.inner.worktrees_root.join(&repo_name);
        std::fs::create_dir_all(&base)?;
        // Auto-generate a name colliding with neither an existing dir nor branch.
        let existing: HashSet<String> = self
            .branches(repo_path)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();
        let mut name = None;
        for attempt in 0..50u64 {
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(attempt)
                .wrapping_add(attempt.wrapping_mul(0x9E37_79B9));
            let candidate = format!(
                "{}-{}",
                ADJECTIVES[(seed % ADJECTIVES.len() as u64) as usize],
                NOUNS[((seed / 31) % NOUNS.len() as u64) as usize]
            );
            if !base.join(&candidate).exists() && !existing.contains(&format!("zeron/{candidate}"))
            {
                name = Some(candidate);
                break;
            }
        }
        let name =
            name.ok_or_else(|| EngineError::Other("Could not allocate a worktree name".into()))?;
        let path = base.join(&name);
        let branch_name = format!("zeron/{name}");
        self.git(
            &[
                "worktree",
                "add",
                "-b",
                &branch_name,
                &path.to_string_lossy(),
                branch,
            ],
            Some(repo_path),
        )
        .await?;
        let checkout = self.checkout_identity(&path).await?;
        Ok(Worktree {
            repo_path: repo_path.to_string_lossy().to_string(),
            path: path.to_string_lossy().to_string(),
            branch: branch_name,
            name,
            checkout_id: Some(checkout.id),
        })
    }

    async fn branch_exists(&self, path: &Path, branch: &str) -> bool {
        self.git(
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            Some(path),
        )
        .await
        .is_ok()
    }

    /// Rename a zeron-created worktree branch after its chat's generated title
    /// (port of zeron's `renameWorktreeBranch`). Guards:
    /// - respect an external checkout/rename: only act while the worktree is still
    ///   on `expected_branch` AND that branch is the original `zeron/<folderName>`;
    /// - a title-slug collision gets a stable 6-hex suffix (hash of the worktree
    ///   path); a collision on THAT too fails.
    ///
    /// Returns the branch the worktree ends up on (re-read after the rename so a
    /// concurrent external checkout always wins the metadata race).
    pub async fn rename_worktree_branch(
        &self,
        worktree_path: &Path,
        expected_branch: &str,
        title: &str,
    ) -> Result<String, EngineError> {
        let current = self.current_branch(worktree_path).await?;
        let folder = worktree_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if current != expected_branch || expected_branch != format!("zeron/{folder}") {
            return Ok(current);
        }
        let preferred = worktree_branch_from_title(title);
        if preferred == current {
            return Ok(current);
        }
        let mut hasher = Sha256::new();
        hasher.update(worktree_path.to_string_lossy().as_bytes());
        let suffix = &hex(&hasher.finalize())[..6];
        let target = if self.branch_exists(worktree_path, &preferred).await {
            format!("{preferred}-{suffix}")
        } else {
            preferred
        };
        if self.branch_exists(worktree_path, &target).await {
            return Err(EngineError::Other(format!(
                "Branch already exists: {target}"
            )));
        }
        self.git(
            &["branch", "-m", "--", &current, &target],
            Some(worktree_path),
        )
        .await?;
        self.current_branch(worktree_path).await
    }

    /// Remove a worktree (if it still exists), then prune stale refs. Deletes the
    /// worktree's branch ONLY when zeron created it (`zeron/…`) — the user may
    /// have checked out their own branch inside the worktree.
    ///
    /// Without `force`, refuses (a [`WORKTREE_UNSAFE_PREFIX`] error naming the
    /// branch and commit count) when removal would discard unmerged commits or
    /// uncommitted changes; nothing is touched in that case.
    pub async fn delete_worktree(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
        force: bool,
    ) -> Result<(), EngineError> {
        let exists = worktree_path.exists();
        let branch = if exists {
            self.current_branch(worktree_path).await.unwrap_or_default()
        } else {
            String::new()
        };
        if !force {
            self.ensure_worktree_safe_to_delete(repo_path, worktree_path, &branch, exists)
                .await?;
        }
        if exists {
            let removed = self
                .git(
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        &worktree_path.to_string_lossy(),
                    ],
                    Some(repo_path),
                )
                .await;
            if removed.is_err() {
                // git refused (or the dir is half-gone) — delete the folder directly.
                let _ = std::fs::remove_dir_all(worktree_path);
            }
        }
        let _ = self.git(&["worktree", "prune"], Some(repo_path)).await;
        if branch.starts_with("zeron/") {
            let _ = self.git(&["branch", "-D", &branch], Some(repo_path)).await;
        }
        Ok(())
    }

    /// Refuse (before anything is removed) when deleting would lose work: commits
    /// on the doomed branch (or a detached HEAD) that no other branch contains,
    /// or uncommitted/untracked files in the worktree. A failing probe is an
    /// error, never "safe".
    async fn ensure_worktree_safe_to_delete(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
        branch: &str,
        worktree_exists: bool,
    ) -> Result<(), EngineError> {
        let mut unmerged = 0u32;
        if branch.starts_with("zeron/") {
            let full = format!("refs/heads/{branch}");
            let exists = self
                .git_probe(
                    &["rev-parse", "--verify", "--quiet", &full],
                    Some(repo_path),
                )
                .await?
                .is_some();
            if exists {
                // `--exclude` globs are relative to the `--branches` namespace.
                let exclude = format!("--exclude={branch}");
                let out = self
                    .git(
                        &[
                            "rev-list",
                            "--count",
                            &full,
                            "--not",
                            &exclude,
                            "--branches",
                            "--remotes",
                        ],
                        Some(repo_path),
                    )
                    .await?;
                unmerged = out.parse().unwrap_or(0);
            }
        } else if branch == "HEAD" && worktree_exists {
            let out = self
                .git(
                    &[
                        "rev-list",
                        "--count",
                        "HEAD",
                        "--not",
                        "--branches",
                        "--remotes",
                    ],
                    Some(worktree_path),
                )
                .await?;
            unmerged = out.parse().unwrap_or(0);
        }
        let dirty = if worktree_exists {
            self.git(
                &["status", "--porcelain", "--untracked-files=normal"],
                Some(worktree_path),
            )
            .await?
            .lines()
            .count()
        } else {
            0
        };
        if unmerged == 0 && dirty == 0 {
            return Ok(());
        }
        let label = if branch == "HEAD" {
            "detached HEAD".to_string()
        } else {
            format!("branch {branch}")
        };
        let mut problems = Vec::new();
        if unmerged > 0 {
            problems.push(format!("{unmerged} unmerged commit(s)"));
        }
        if dirty > 0 {
            problems.push(format!("{dirty} uncommitted change(s)"));
        }
        Err(EngineError::Other(format!(
            "{WORKTREE_UNSAFE_PREFIX} {label} has {}; pass force to discard",
            problems.join(" and ")
        )))
    }

    // ── merge preview ───────────────────────────────────────────────────────

    /// Read-only preview of merging the branch checked out in `worktree` into
    /// the lead branch of `base_repo` (`base_override`, else the branch checked
    /// out there). Changes no tree, index, HEAD or ref.
    pub async fn merge_preview(
        &self,
        worktree: &Path,
        base_repo: &Path,
        base_override: Option<&str>,
    ) -> Result<MergePreview, EngineError> {
        let version = self.git_version().await;
        // Unknown version: try anyway, a usage error (129) falls back below.
        let supported = version
            .as_ref()
            .is_none_or(|(major, minor, _)| (*major, *minor) >= MERGE_TREE_WRITE_TREE_MIN);
        self.merge_preview_with(
            worktree,
            base_repo,
            base_override,
            supported,
            version.map(|(_, _, text)| text),
        )
        .await
    }

    /// [`Self::merge_preview`] with the `merge-tree --write-tree` capability
    /// given (tests force the old-git fallback).
    pub(crate) async fn merge_preview_with(
        &self,
        worktree: &Path,
        base_repo: &Path,
        base_override: Option<&str>,
        conflict_check_supported: bool,
        git_version: Option<String>,
    ) -> Result<MergePreview, EngineError> {
        if !Self::path_exists(worktree).await || !self.is_repo(worktree).await {
            return Err(EngineError::Other(
                "this session has no worktree yet".into(),
            ));
        }
        // Resolve the lead first, in the project folder: a bare `HEAD` inside the
        // worktree is the member's own head. Both sides become full shas before
        // `merge-tree`, which exits 1 for an unknown ref just as for conflicts.
        let base_ref = match base_override.map(str::trim).filter(|b| !b.is_empty()) {
            Some(b) if b.starts_with('-') => {
                return Err(EngineError::Other(format!("lead branch {b} not found")));
            }
            Some(b) => b.to_string(),
            None => self.current_branch(base_repo).await?,
        };
        let base_head = self
            .git(
                &["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")],
                Some(base_repo),
            )
            .await
            .map_err(|_| EngineError::Other(format!("lead branch {base_ref} not found")))?;
        let branch_head = self
            .git(&["rev-parse", "--verify", "HEAD^{commit}"], Some(worktree))
            .await?;
        let branch = match self.current_branch(worktree).await? {
            b if b == "HEAD" => branch_head.chars().take(7).collect(),
            b => b,
        };

        // `left` = only on the lead (behind), `right` = only on the member (ahead).
        let counts = self
            .git(
                &[
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("{base_head}...{branch_head}"),
                ],
                Some(worktree),
            )
            .await?;
        let mut counts = counts
            .split_whitespace()
            .map(|n| n.parse::<usize>().unwrap_or(0));
        let behind = counts.next().unwrap_or(0);
        let ahead = counts.next().unwrap_or(0);

        let mut preview = MergePreview {
            chat_id: String::new(),
            worktree_path: worktree.to_string_lossy().to_string(),
            branch,
            branch_head: branch_head.clone(),
            base: base_ref,
            base_head: base_head.clone(),
            merge_base: None,
            ahead,
            behind,
            already_merged: ahead == 0,
            fast_forward: behind == 0 && ahead > 0,
            files: Vec::new(),
            files_total: 0,
            files_truncated: false,
            uncommitted: MergePreviewDirty::default(),
            conflict_check: MergeConflictCheck::Clean,
            conflicts: Vec::new(),
            conflicts_total: 0,
            merge_tree: None,
            note: None,
            git_version,
        };

        // Uncommitted edits. `--no-optional-locks` keeps `status` from refreshing
        // (and locking) the index under the member's own git.
        let (code, out, err) = self
            .git_status(
                &[
                    "--no-optional-locks",
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "-uall",
                ],
                Some(worktree),
            )
            .await?;
        if code == 0 {
            preview.uncommitted = parse_porcelain_status(&out);
        } else {
            preview.note = Some(format!("uncommitted edits not read: {}", err.trim()));
        }

        let (code, out, _) = self
            .git_status(&["merge-base", &base_head, &branch_head], Some(worktree))
            .await?;
        let merge_base = out.trim().to_string();
        if code != 0 || merge_base.is_empty() {
            preview.conflict_check = MergeConflictCheck::Failed;
            preview.note = Some(
                "the member branch and the lead share no history, so there is nothing to merge"
                    .into(),
            );
            return Ok(preview);
        }
        preview.merge_base = Some(merge_base.clone());

        let (code, out, err) = self
            .git_status(
                &[
                    "diff",
                    "--name-status",
                    "-z",
                    "--find-renames",
                    &merge_base,
                    &branch_head,
                ],
                Some(worktree),
            )
            .await?;
        if code == 0 {
            let mut files = parse_name_status(&out);
            preview.files_total = files.len();
            if files.len() > MERGE_PREVIEW_MAX_FILES {
                files.truncate(MERGE_PREVIEW_MAX_FILES);
                preview.files_truncated = true;
            }
            preview.files = files;
        } else {
            preview.note = Some(format!("files not listed: {}", err.trim()));
        }

        if !conflict_check_supported {
            preview.conflict_check = MergeConflictCheck::Unavailable;
            preview.note = Some(unavailable_note(preview.git_version.as_deref()));
            return Ok(preview);
        }
        // `--write-tree` writes loose tree/blob objects for the merge result into
        // the object database (harmless, gc-able) but never touches the index,
        // any working tree, HEAD or refs.
        let (code, out, err) = self
            .git_status(
                &[
                    "merge-tree",
                    "--write-tree",
                    "-z",
                    "--name-only",
                    "--no-messages",
                    &base_head,
                    &branch_head,
                ],
                Some(worktree),
            )
            .await?;
        match code {
            0 | 1 => {
                let mut parts = out.split('\0');
                let tree = parts.next().unwrap_or("").trim();
                if !tree.is_empty() {
                    preview.merge_tree = Some(tree.to_string());
                }
                let mut conflicts: Vec<String> = Vec::new();
                for path in parts.filter(|p| !p.is_empty()) {
                    if !conflicts.iter().any(|c| c == path) {
                        conflicts.push(path.to_string());
                    }
                }
                preview.conflicts_total = conflicts.len();
                conflicts.truncate(MERGE_PREVIEW_MAX_CONFLICTS);
                preview.conflicts = conflicts;
                preview.conflict_check = if code == 0 {
                    MergeConflictCheck::Clean
                } else {
                    MergeConflictCheck::Conflicts
                };
            }
            129 => {
                preview.conflict_check = MergeConflictCheck::Unavailable;
                preview.note = Some(unavailable_note(preview.git_version.as_deref()));
            }
            _ => {
                preview.conflict_check = MergeConflictCheck::Failed;
                let err = err.trim();
                preview.note = Some(if err.is_empty() {
                    format!("git merge-tree failed (exit {code})")
                } else {
                    format!("git merge-tree failed: {err}")
                });
            }
        }
        Ok(preview)
    }

    // ── ListFolders ─────────────────────────────────────────────────────────

    /// One directory level (home by default): dotfiles hidden, directories first,
    /// capped at [`FOLDER_LIST_MAX_ENTRIES`] with a `truncated` flag. The walk runs
    /// in a spawned blocking task under a 6s wall-clock ceiling — a wedged path
    /// (dead mount, permission-gated folder) fails this listing without blocking
    /// anything else; the abandoned task unwinds on its own thread.
    pub async fn list_folders(&self, path: Option<String>) -> Result<FolderListing, EngineError> {
        self.list_folders_with(path, FOLDER_LIST_TIMEOUT, false)
            .await
    }

    // ── SearchHomeFolders ───────────────────────────────────────────────────

    /// Folders anywhere under home whose names match `query`, from the home
    /// workspace search index — the one projectless chats use, so it skips
    /// dot folders, dependency and cache folders, and each repository's
    /// ignored paths. An empty query only starts the index.
    pub async fn search_home_folders(
        &self,
        query: String,
        limit: usize,
    ) -> Result<HomeFolderSearchResult, EngineError> {
        let search = self.inner.workspace_search.clone();
        let worker = disposable_worker("home-folder-search", move || {
            search_home_folders_blocking(&search, &query, limit)
        });
        match tokio::time::timeout(FILE_SEARCH_TIMEOUT, worker).await {
            Ok(Some(result)) => result,
            Ok(None) => Err(EngineError::Other("folder search worker exited".into())),
            Err(_) => Err(EngineError::Other("folder search timed out".into())),
        }
    }

    /// The new-project picker is done with the home index: drop it now
    /// unless a projectless chat warmed it.
    pub fn release_home_folder_search(&self) {
        if let Ok(home) = home_search_root() {
            self.inner.workspace_search.release_unless_warmed(&home);
        }
    }

    /// Search a checkout's files and directories by fuzzy relative path,
    /// from the workspace search index (fff), which honors `.gitignore`,
    /// `.ignore`, and global git excludes. An empty query lists the chat's
    /// featured paths first, then recently changed files.
    pub async fn search_files(
        &self,
        root: PathBuf,
        query: String,
        featured_paths: Vec<String>,
    ) -> Result<Vec<FileSearchMatch>, EngineError> {
        let search = self.inner.workspace_search.clone();
        let worker = disposable_worker("file-search", move || {
            search_files_blocking(&search, &root, &query, &featured_paths)
        });
        match tokio::time::timeout(FILE_SEARCH_TIMEOUT, worker).await {
            Ok(Some(result)) => result,
            Ok(None) => Err(EngineError::Other("file search worker exited".into())),
            Err(_) => Err(EngineError::Other("file search timed out".into())),
        }
    }

    /// `hang_for_test` makes the worker never respond — exercises the timeout path.
    ///
    /// The walk runs on a DETACHED OS thread (not the tokio blocking pool): a
    /// readdir wedged in the kernel can't be cancelled, and a poisoned blocking
    /// pool — or a runtime shutdown waiting on it — must never be possible. On
    /// timeout the thread is simply abandoned (the zeron backend's disposable
    /// worker, minus the terminate()).
    #[doc(hidden)]
    pub async fn list_folders_with(
        &self,
        path: Option<String>,
        timeout: Duration,
        hang_for_test: bool,
    ) -> Result<FolderListing, EngineError> {
        let target = match path.filter(|p| !p.trim().is_empty()) {
            Some(p) => absolutize(Path::new(&p)),
            None => session_home_dir().map_err(|error| EngineError::Other(error.to_string()))?,
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("folder-list".into())
            .spawn(move || {
                if hang_for_test {
                    // Hold the sender without responding (detached thread; process
                    // exit reclaims it) — the caller must hit its timeout.
                    std::thread::sleep(Duration::from_secs(3600));
                }
                let _ = tx.send(list_folders_blocking(&target));
            });
        if let Err(err) = spawned {
            return Err(EngineError::Other(format!("folder listing failed: {err}")));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(EngineError::Other("folder listing worker exited".into())),
            Err(_) => Err(EngineError::Other(
                "folder listing timed out on the device".into(),
            )),
        }
    }

    // ── ListDrives ──────────────────────────────────────────────────────────

    /// Mounted drives/volumes — the browse roots beyond home. Same disposable
    /// worker + wall-clock ceiling as `ListFolders`: a dead network mount can
    /// wedge the probe (macOS `canonicalize` stats each volume), and that must
    /// fail this listing, not the runtime.
    pub async fn list_drives(&self) -> Result<Vec<DriveEntry>, EngineError> {
        let worker = disposable_worker("drive-list", list_drives_blocking);
        match tokio::time::timeout(FOLDER_LIST_TIMEOUT, worker).await {
            Ok(Some(drives)) => Ok(drives),
            Ok(None) => Err(EngineError::Other("drive listing worker exited".into())),
            Err(_) => Err(EngineError::Other(
                "drive listing timed out on the device".into(),
            )),
        }
    }

    // ── RegisterProject ─────────────────────────────────────────────────────

    /// The folders a project `query` (a path, or a short name like `t3code`)
    /// may mean on this device, see [`find_project_folders`]. The probing runs
    /// in a disposable worker under the folder-listing ceiling, so a dead
    /// mount fails this lookup and nothing else. `Err` is a message for the
    /// caller: a refused folder, or a search that could not run.
    pub async fn resolve_project_folder(
        &self,
        query: &str,
        known: Vec<KnownFolder>,
    ) -> Result<ProjectSearch, String> {
        let home = session_home_dir().map_err(str::to_string)?;
        // Projects sit at the top of a Windows drive; a Unix mount point is
        // not somewhere to look for a name.
        let drives: Vec<PathBuf> = if cfg!(windows) {
            self.list_drives()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|drive| PathBuf::from(drive.path))
                .collect()
        } else {
            Vec::new()
        };
        let owned = query.to_owned();
        let at_home = home.clone();
        let worker = disposable_worker("project-folder", move || {
            find_project_folders(&owned, &known, &at_home, &drives)
        });
        let mut search = match tokio::time::timeout(FOLDER_LIST_TIMEOUT, worker).await {
            Ok(Some(found)) => found?,
            Ok(None) => return Err("the project folder search worker exited".into()),
            Err(_) => return Err("the project folder search timed out on the device".into()),
        };
        // Last, the home index: a folder nested somewhere under home.
        if search.candidates.is_empty() && !looks_like_path(query) {
            let name = query.trim().to_lowercase();
            if let Ok(found) = self.search_home_folders(query.trim().to_owned(), 8).await {
                for hit in found.matches {
                    let path = PathBuf::from(&hit.path);
                    let same_name = hit
                        .relative
                        .trim_end_matches(['/', '\\'])
                        .rsplit(['/', '\\'])
                        .next()
                        .is_some_and(|last| last.to_lowercase() == name);
                    if same_name
                        && unsafe_project_root(&path, &home).is_none()
                        && !search
                            .candidates
                            .iter()
                            .any(|c| path_key(Path::new(&c.path)) == path_key(&path))
                    {
                        search.candidates.push(ProjectFolder {
                            path: hit.path,
                            source: "index",
                            is_repo: hit.is_repo,
                        });
                    }
                }
                search.searched.push("the home folder index".into());
                search.incomplete = found.indexing && search.candidates.is_empty();
            }
            self.release_home_folder_search();
        }
        Ok(search)
    }
}

async fn disposable_worker<T: Send + 'static>(
    name: &'static str,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .ok()?;
    rx.await.ok()
}

/// First git that has `merge-tree --write-tree`.
const MERGE_TREE_WRITE_TREE_MIN: (u32, u32) = (2, 38);
const MERGE_PREVIEW_MAX_FILES: usize = 200;
const MERGE_PREVIEW_MAX_CONFLICTS: usize = 100;
const MERGE_PREVIEW_MAX_DIRTY_PATHS: usize = 50;

fn unavailable_note(version: Option<&str>) -> String {
    format!(
        "git {} is older than 2.38, which merge-tree --write-tree needs; conflicts not checked. \
         Files touched, ahead/behind and uncommitted are still accurate.",
        version.unwrap_or("(unknown)")
    )
}

/// `git version 2.43.0`, `2.39.5 (Apple Git-154)`, `2.37.1.windows.1` ->
/// `(major, minor, "2.43.0" | "2.39.5" | "2.37.1.windows.1")`.
fn parse_git_version(text: &str) -> Option<(u32, u32, String)> {
    let text = text.trim();
    let rest = text.strip_prefix("git version").unwrap_or(text).trim();
    let token = rest.split_whitespace().next()?;
    let mut numbers = token.split('.');
    let major = numbers.next()?.parse().ok()?;
    let minor = numbers.next()?.parse().ok()?;
    Some((major, minor, token.to_string()))
}

/// `diff --name-status -z`: `<status>\0<path>\0`, with two paths for `R`/`C`.
fn parse_name_status(out: &str) -> Vec<MergePreviewFile> {
    let mut tokens = out.split('\0').filter(|t| !t.is_empty());
    let mut files = Vec::new();
    while let Some(status) = tokens.next() {
        let letter = status.chars().next().unwrap_or('M');
        let (path, old_path) = if matches!(letter, 'R' | 'C') {
            let old = tokens.next();
            (tokens.next(), old)
        } else {
            (tokens.next(), None)
        };
        let Some(path) = path else { break };
        files.push(MergePreviewFile {
            path: path.to_string(),
            status: letter.to_string(),
            old_path: old_path.map(str::to_string),
        });
    }
    files
}

/// `status --porcelain=v1 -z`: `XY path\0`, renames/copies add a source path.
fn parse_porcelain_status(out: &str) -> MergePreviewDirty {
    let mut dirty = MergePreviewDirty::default();
    let mut tokens = out.split('\0').filter(|t| !t.is_empty());
    while let Some(entry) = tokens.next() {
        let mut chars = entry.chars();
        let (Some(x), Some(y)) = (chars.next(), chars.next()) else {
            continue;
        };
        let path = entry.get(3..).unwrap_or("");
        if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            tokens.next();
        }
        if x == '!' {
            continue;
        }
        if x == '?' {
            dirty.untracked += 1;
        } else {
            if matches!(x, 'M' | 'A' | 'D' | 'R' | 'C' | 'T') {
                dirty.staged += 1;
            }
            if matches!(y, 'M' | 'D' | 'T') || x == 'U' || y == 'U' {
                dirty.unstaged += 1;
            }
        }
        dirty.total += 1;
        if dirty.paths.len() < MERGE_PREVIEW_MAX_DIRTY_PATHS {
            dirty.paths.push(path.to_string());
        }
    }
    dirty
}

/// The blocking walk: ONE readdir of the target; `is_repo` is a cheap `.git`
/// existence probe per directory entry.
fn list_folders_blocking(target: &Path) -> Result<FolderListing, EngineError> {
    let read = std::fs::read_dir(target).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            EngineError::Other("Zeron doesn't have access to this folder on the device.".into())
        }
        _ => EngineError::Other(format!("could not read that folder: {e}")),
    })?;
    let mut entries: Vec<FolderEntry> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let is_repo = is_dir && entry.path().join(".git").exists();
        entries.push(FolderEntry {
            name,
            is_dir,
            is_repo,
        });
    }
    // Directories first, each group name-sorted (case-insensitive).
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let truncated = entries.len() > FOLDER_LIST_MAX_ENTRIES;
    entries.truncate(FOLDER_LIST_MAX_ENTRIES);
    Ok(FolderListing {
        path: folder_browser_path(target),
        entries,
        truncated,
    })
}

/// The blocking drive walk. Best-effort everywhere: the list is navigation
/// sugar for the folder browser, so an unreadable source means fewer rows,
/// never an error (home always remains reachable without it).
fn list_drives_blocking() -> Vec<DriveEntry> {
    #[cfg(target_os = "macos")]
    let drives = macos_drives();
    #[cfg(target_os = "linux")]
    let drives = linux_drives(&std::fs::read_to_string("/proc/mounts").unwrap_or_default());
    #[cfg(windows)]
    let drives = windows_drives();
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    let drives = Vec::new();
    finish_drives(drives)
}

/// `/Volumes` holds every mounted volume; the boot volume is a symlink to `/`,
/// which `canonicalize` resolves — so the system drive arrives under its real
/// name ("Macintosh HD") and the fallback "System" row never shows on macOS.
#[cfg(target_os = "macos")]
fn macos_drives() -> Vec<DriveEntry> {
    let mut drives: Vec<DriveEntry> = Vec::new();
    if let Ok(read) = std::fs::read_dir("/Volumes") {
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            // Follows the symlink — a dangling one (mid-eject) drops out here.
            if !path.is_dir() {
                continue;
            }
            let resolved = std::fs::canonicalize(&path).unwrap_or(path);
            drives.push(DriveEntry {
                name,
                path: resolved.to_string_lossy().to_string(),
            });
        }
    }
    drives
}

/// Top-level directories the FHS (or its de-facto extensions) owns. A
/// depth-one mount point OUTSIDE this set is a user-created drive root
/// (`/disk2`, `/data`, `/tank`) — the system's own split partitions
/// (`/boot`, a separate `/var` or `/home`) are plumbing, not drives.
#[cfg(any(target_os = "linux", test))]
const FHS_TOP_LEVEL: &[&str] = &[
    "bin",
    "boot",
    "dev",
    "efi",
    "etc",
    "home",
    "lib",
    "lib32",
    "lib64",
    "libx32",
    "lost+found",
    "media",
    "mnt",
    "nix",
    "opt",
    "proc",
    "root",
    "run",
    "sbin",
    "snap",
    "srv",
    "sys",
    "tmp",
    "usr",
    "var",
];

/// Mounted drives from `/proc/mounts`: the conventional removable locations
/// (`/media`, `/run/media`, `/mnt` — udisks, WSL drive letters, manual
/// mounts, whatever the filesystem) plus block-device partitions mounted at
/// custom top-level paths like `/disk2` (PR #144 feedback) — `/dev/*`
/// sources at depth-one non-FHS points, minus squashfs/erofs images (snaps).
/// Plus "System" for `/`: anything mounted deeper stays reachable by
/// browsing from the root, and the palette accepts typed paths besides.
#[cfg(any(target_os = "linux", test))]
fn linux_drives(mounts: &str) -> Vec<DriveEntry> {
    let mut drives: Vec<DriveEntry> = Vec::new();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(source), Some(point), fstype) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let point = unescape_mount_point(point);
        let conventional = point.starts_with("/media/")
            || point.starts_with("/run/media/")
            || point == "/mnt"
            || point.starts_with("/mnt/");
        let custom_block = source.starts_with("/dev/")
            && !matches!(fstype, Some("squashfs") | Some("erofs"))
            && point
                .strip_prefix('/')
                .is_some_and(|p| !p.is_empty() && !p.contains('/') && !FHS_TOP_LEVEL.contains(&p));
        if !conventional && !custom_block {
            continue;
        }
        let name = point
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("Drive")
            .to_string();
        drives.push(DriveEntry { name, path: point });
    }
    drives.push(DriveEntry {
        name: "System".into(),
        path: "/".into(),
    });
    drives
}

/// Probe the drive letters. `exists` on a wedged network letter can hang —
/// covered by the caller's disposable-worker timeout.
#[cfg(windows)]
fn windows_drives() -> Vec<DriveEntry> {
    ('A'..='Z')
        .filter_map(|letter| {
            let path = format!("{letter}:\\");
            std::path::Path::new(&path).exists().then(|| DriveEntry {
                name: format!("{letter}:"),
                path,
            })
        })
        .collect()
}

/// Dedupe by mount point (first mention wins — the named row beats a
/// late generic duplicate), system root first, then name order, capped.
fn finish_drives(mut drives: Vec<DriveEntry>) -> Vec<DriveEntry> {
    let mut seen = HashSet::new();
    drives.retain(|d| seen.insert(d.path.clone()));
    drives.sort_by(|a, b| {
        (a.path != "/")
            .cmp(&(b.path != "/"))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    drives.truncate(DRIVE_LIST_MAX_ENTRIES);
    drives
}

/// `/proc/mounts` octal-escapes whitespace in mount points (`\040` = space).
#[cfg(any(target_os = "linux", test))]
fn unescape_mount_point(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let digits: String = chars.clone().take(3).collect();
            if digits.len() == 3
                && let Ok(code) = u8::from_str_radix(&digits, 8)
            {
                out.push(code as char);
                chars.nth(2);
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Case-insensitive subsequence score. Lower is better: adjacent and earlier
/// characters win, while still allowing `cmp rs` to find `composer.rs`.
fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    let candidate = candidate.to_lowercase();
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .try_fold(0usize, |total, term| {
            let mut at = 0;
            let mut score = 0usize;
            let mut previous_end = None;
            for needle in term.chars() {
                let found = candidate[at..].find(needle)? + at;
                score += found;
                if previous_end == Some(found) {
                    score = score.saturating_sub(2);
                }
                at = found + needle.len_utf8();
                previous_end = Some(at);
            }
            Some(total.saturating_add(score))
        })
}

/// Match a history commit using the same semantics as the history UI.
///
/// This remains the shared entry point so RPC filtering and client-side
/// filtering cannot disagree about Unicode case normalization.
pub fn git_history_matches(query: &str, commit: &GitHistoryCommit) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    let normalized = query.to_ascii_lowercase();
    commit.sha.to_ascii_lowercase().starts_with(&normalized)
        || fuzzy_score(query, &format!("{} {}", commit.sha, commit.subject)).is_some()
}

/// Contract hidden commits to their nearest visible ancestors. Search results
/// remain sparse without leaving graph lanes aimed at rows that are absent.
fn compact_history_commits(
    commits: &[GitHistoryCommit],
    visible: &HashSet<String>,
) -> Vec<GitHistoryCommit> {
    let by_sha: HashMap<_, _> = commits
        .iter()
        .map(|commit| (commit.sha.as_str(), commit))
        .collect();

    /// Resolve a hidden commit to its nearest visible ancestors without using
    /// the call stack. Completed nodes are cached across visible commits so a
    /// shared hidden branch is contracted only once.
    fn nearest_visible_parents(
        sha: &str,
        visible: &HashSet<String>,
        by_sha: &HashMap<&str, &GitHistoryCommit>,
        memo: &mut HashMap<String, Vec<String>>,
    ) -> Vec<String> {
        if visible.contains(sha) || !by_sha.contains_key(sha) {
            return vec![sha.to_string()];
        }
        if let Some(cached) = memo.get(sha) {
            return cached.clone();
        }

        struct Frame {
            sha: String,
            next_parent: usize,
            resolved: Vec<String>,
            seen: HashSet<String>,
        }

        impl Frame {
            fn new(sha: String) -> Self {
                Self {
                    sha,
                    next_parent: 0,
                    resolved: Vec::new(),
                    seen: HashSet::new(),
                }
            }

            fn extend(&mut self, parents: &[String]) {
                for parent in parents {
                    if self.seen.insert(parent.clone()) {
                        self.resolved.push(parent.clone());
                    }
                }
            }
        }

        let mut visiting = HashSet::from([sha.to_string()]);
        let mut stack = vec![Frame::new(sha.to_string())];
        loop {
            let next_parent = {
                let frame = stack.last_mut().expect("history traversal frame");
                let parents = &by_sha[frame.sha.as_str()].parent_shas;
                (frame.next_parent < parents.len()).then(|| {
                    let parent = parents[frame.next_parent].clone();
                    frame.next_parent += 1;
                    parent
                })
            };

            let Some(parent) = next_parent else {
                let frame = stack.pop().expect("history traversal frame");
                visiting.remove(&frame.sha);
                let resolved = frame.resolved;
                memo.insert(frame.sha, resolved.clone());
                if let Some(caller) = stack.last_mut() {
                    caller.extend(&resolved);
                    continue;
                }
                return resolved;
            };

            let resolved = if visible.contains(&parent) || !by_sha.contains_key(parent.as_str()) {
                Some(vec![parent.clone()])
            } else if let Some(cached) = memo.get(&parent) {
                Some(cached.clone())
            } else if visiting.contains(&parent) {
                // Git commit graphs are acyclic, but keep malformed input from
                // looping forever just as the previous `visiting` guard did.
                Some(Vec::new())
            } else {
                None
            };

            if let Some(resolved) = resolved {
                stack
                    .last_mut()
                    .expect("history traversal frame")
                    .extend(&resolved);
            } else {
                visiting.insert(parent.clone());
                stack.push(Frame::new(parent));
            }
        }
    }

    let mut memo = HashMap::new();
    commits
        .iter()
        .filter(|commit| visible.contains(&commit.sha))
        .cloned()
        .map(|mut commit| {
            let mut seen = HashSet::new();
            commit.parent_shas = commit
                .parent_shas
                .iter()
                .flat_map(|parent| nearest_visible_parents(parent, visible, &by_sha, &mut memo))
                .filter(|parent| seen.insert(parent.clone()))
                .collect();
            commit
        })
        .collect()
}

fn search_files_blocking(
    search: &WorkspaceSearch,
    root: &Path,
    query: &str,
    featured_paths: &[String],
) -> Result<Vec<FileSearchMatch>, EngineError> {
    let root = canonical_search_root(root)?;
    let mut matches: Vec<FileSearchMatch> = Vec::with_capacity(FILE_SEARCH_MAX_RESULTS);
    if query.trim().is_empty() {
        for path in featured_paths {
            if matches.len() == FILE_SEARCH_MAX_RESULTS {
                break;
            }
            let path = Path::new(path);
            let full = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            let Ok(canonical) = std::fs::canonicalize(full) else {
                continue;
            };
            let Ok(relative) = canonical.strip_prefix(&root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if relative.is_empty()
                || is_git_metadata(&relative)
                || matches.iter().any(|found| found.path == relative)
            {
                continue;
            }
            matches.push(FileSearchMatch {
                is_dir: canonical.is_dir(),
                path: relative,
            });
        }
    }
    let found = search
        .search_names_within(
            &root,
            query,
            FILE_SEARCH_MAX_RESULTS + matches.len(),
            NameKinds::FilesAndDirectories,
            crate::workspace_search::NAME_SCAN_WAIT,
        )
        .map_err(|e| EngineError::Other(e.to_string()))?;
    for found in found.matches {
        if matches.len() == FILE_SEARCH_MAX_RESULTS {
            break;
        }
        if is_git_metadata(&found.path)
            || matches.iter().any(|known| known.path == found.path)
            || crate::workspace_search::escapes_through_symlink(&root, &found.path, true)
        {
            continue;
        }
        matches.push(FileSearchMatch {
            path: found.path,
            is_dir: found.is_dir,
        });
    }
    Ok(matches)
}

/// The home folder as the workspace search index keys it.
fn home_search_root() -> Result<PathBuf, EngineError> {
    let home = session_home_dir().map_err(|error| EngineError::Other(error.to_string()))?;
    canonical_search_root(&home)
}

fn search_home_folders_blocking(
    search: &WorkspaceSearch,
    query: &str,
    limit: usize,
) -> Result<HomeFolderSearchResult, EngineError> {
    let mut found = search_folders_under(search, &home_search_root()?, query, limit)?;
    // The index keys Home by its canonical path; clients compare results
    // with Home and existing projects as the folder browser spells them. A
    // symlinked Home (`/home/me` → `/data/home/me`) must not read as a
    // second copy of every folder.
    if let Ok(home) = session_home_dir() {
        for found in &mut found.matches {
            found.path = folder_browser_path(&home.join(&found.relative));
        }
    }
    Ok(found)
}

/// `SearchHomeFolders` against an already-resolved (canonical) root.
fn search_folders_under(
    search: &WorkspaceSearch,
    home: &Path,
    query: &str,
    limit: usize,
) -> Result<HomeFolderSearchResult, EngineError> {
    let query = normalize_name_query(query);
    let query = query.as_ref();
    // fff scores a `/` query against whole paths, where a folder loses to
    // its many descendants; ask for the last segment's folders instead and
    // let `rank_home_folders` check the leading segments.
    let (index_query, candidates) = match query.trim().trim_end_matches('/').rsplit_once('/') {
        Some((_, last)) if !last.is_empty() => (last, HOME_FOLDER_PATH_CANDIDATES),
        _ => (query, HOME_FOLDER_CANDIDATES),
    };
    let found = search
        .search_names(home, index_query, candidates, NameKinds::Directories)
        .map_err(|e| EngineError::Other(e.to_string()))?;
    // A `.git` probe per candidate (a few hundred stats) lets repositories
    // rank first.
    let candidates = found
        .matches
        .into_iter()
        .filter(|found| !is_git_metadata(&found.path))
        .map(|found| {
            let is_repo = home.join(&found.path).join(".git").exists();
            (found.path, is_repo)
        })
        .collect();
    let matches = rank_home_folders(query, candidates)
        .into_iter()
        .take(limit)
        .map(|(relative, is_repo)| HomeFolderMatch {
            path: folder_browser_path(&home.join(&relative)),
            relative,
            is_repo,
        })
        .collect();
    Ok(HomeFolderSearchResult {
        matches,
        indexing: found.indexing,
    })
}

/// Keep canonical index keys internal. Ordinary Windows paths are easier for
/// browser clients to compare with Home and existing spaces; retain extended
/// paths when their length or names require them, and for UNC shares.
fn folder_browser_path(path: &Path) -> String {
    #[cfg(windows)]
    let path = dunce::simplified(path);
    path.to_string_lossy().into_owned()
}

/// Re-rank fuzzy folder candidates `(relative path, is a repository)`, given
/// in index order, by how well the folder itself matches: an exact name,
/// then a name starting with the query, then one containing it. A query
/// with `/` matches the path's last segments instead, each query segment a
/// prefix of its path segment (`dev/com` → `…/development/comet`). Within a
/// tier repositories go first, then shallower folders. Candidates matching
/// only through a typo or a parent's name are kept only when nothing better
/// matched, ordered the same way.
fn rank_home_folders(query: &str, candidates: Vec<(String, bool)>) -> Vec<(String, bool)> {
    let query = query.trim().to_lowercase();
    let segments: Vec<&str> = query.split('/').filter(|s| !s.is_empty()).collect();
    let tier = |relative: &str| -> u8 {
        let relative = relative.to_lowercase();
        let path: Vec<&str> = relative.split('/').collect();
        if query.contains('/') {
            if segments.is_empty() || segments.len() > path.len() {
                return 0;
            }
            let tail = &path[path.len() - segments.len()..];
            if !tail
                .iter()
                .zip(&segments)
                .all(|(seg, q)| seg.starts_with(q))
            {
                return 0;
            }
            return if tail.last() == segments.last() { 3 } else { 2 };
        }
        let name = path.last().copied().unwrap_or_default();
        if name == query {
            3
        } else if name.starts_with(&query) {
            2
        } else {
            u8::from(name.contains(&query))
        }
    };
    let mut ranked: Vec<(u8, usize, usize, (String, bool))> = candidates
        .into_iter()
        .enumerate()
        .map(|(order, (relative, is_repo))| {
            let depth = relative.matches('/').count();
            (tier(&relative), depth, order, (relative, is_repo))
        })
        .collect();
    if ranked.iter().any(|(tier, ..)| *tier > 0) {
        ranked.retain(|(tier, ..)| *tier > 0);
    }
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.3.1.cmp(&a.3.1))
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
    });
    ranked.into_iter().map(|(.., found)| found).collect()
}

/// Repository metadata is never offered, even if an index surfaced it.
fn is_git_metadata(relative: &str) -> bool {
    relative.split('/').any(|component| component == ".git")
}

fn canonical_search_root(root: &Path) -> Result<PathBuf, EngineError> {
    std::fs::canonicalize(root)
        .map_err(|e| EngineError::Other(format!("could not search workspace: {e}")))
}

/// Turn a generated chat title into the semantic portion of a Zeron branch
/// (port of zeron's `worktreeBranchFromTitle`). Zeron NFKD-normalizes accented
/// letters first; native keeps it ASCII-only (generated titles are Title Case
/// English), so non-ASCII characters collapse into the `-` separator.
pub fn worktree_branch_from_title(title: &str) -> String {
    let mut slug = String::new();
    for c in title.trim().chars() {
        if matches!(c, '\'' | '"' | '`') {
            continue; // dropped entirely (cafe's → cafes), not a separator
        }
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(48);
    let slug = slug.trim_matches('-');
    format!("zeron/{}", if slug.is_empty() { "update" } else { slug })
}

fn bounded_field(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[derive(serde::Deserialize)]
struct GitHubCommitAvatar {
    sha: String,
    author: Option<GitHubAvatarUser>,
    commit: GitHubCommitMetadata,
}

#[derive(serde::Deserialize)]
struct GitHubAvatarUser {
    avatar_url: String,
}

#[derive(serde::Deserialize)]
struct GitHubCommitMetadata {
    author: GitHubCommitAuthor,
}

#[derive(serde::Deserialize)]
struct GitHubCommitAuthor {
    email: String,
}

fn parse_github_remote(remote: &str) -> Option<(String, String)> {
    let remote = remote.trim();
    let path = remote
        .strip_prefix("https://github.com/")
        .or_else(|| remote.strip_prefix("http://github.com/"))
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.strip_prefix("git@github.com:"))?;
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty()
        || repo.is_empty()
        || parts.next().is_some()
        || !owner
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        || !repo.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

fn parse_history_log(
    output: &str,
    refs_by_sha: &HashMap<String, Vec<GitHistoryRef>>,
) -> Vec<GitHistoryCommit> {
    let fields: Vec<&str> = output.split('\0').collect();
    fields
        .chunks(6)
        .filter_map(|record| {
            if record.len() != 6 {
                return None;
            }
            let sha = record[0].trim_start_matches(['\r', '\n']);
            if sha.is_empty() {
                return None;
            }
            Some(GitHistoryCommit {
                sha: sha.to_string(),
                parent_shas: record[1]
                    .split_ascii_whitespace()
                    .map(str::to_string)
                    .collect(),
                subject: bounded_field(record[2], 4_096),
                author_name: bounded_field(record[3], 512),
                author_email: bounded_field(record[4], 512),
                authored_at: bounded_field(record[5], 128),
                refs: refs_by_sha.get(sha).cloned().unwrap_or_default(),
            })
        })
        .collect()
}

fn parse_history_refs(output: &str) -> HashMap<String, Vec<GitHistoryRef>> {
    let fields: Vec<&str> = output.split('\0').collect();
    let mut refs_by_sha: HashMap<String, Vec<GitHistoryRef>> = HashMap::new();
    for record in fields.chunks(6) {
        if record.len() != 6 {
            continue;
        }
        let full_name = record[0].trim_start_matches(['\r', '\n']);
        let object_sha = record[1];
        let object_type = record[2];
        let peeled_sha = record[3];
        let peeled_type = record[4];
        let symbolic_target = record[5];
        if full_name.is_empty() || !symbolic_target.is_empty() {
            continue;
        }
        let Some((kind, label)) = (if let Some(label) = full_name.strip_prefix("refs/heads/") {
            Some((GitHistoryRefKind::Branch, label))
        } else if let Some(label) = full_name.strip_prefix("refs/remotes/") {
            Some((GitHistoryRefKind::Remote, label))
        } else {
            full_name
                .strip_prefix("refs/tags/")
                .map(|label| (GitHistoryRefKind::Tag, label))
        }) else {
            continue;
        };
        let target_sha = if object_type == "commit" {
            object_sha
        } else if object_type == "tag" && peeled_type == "commit" {
            peeled_sha
        } else {
            continue;
        };
        refs_by_sha
            .entry(target_sha.to_string())
            .or_default()
            .push(GitHistoryRef {
                kind,
                label: bounded_field(label, 1_024),
            });
    }
    for refs in refs_by_sha.values_mut() {
        refs.sort_by(|left, right| {
            let order = |kind| match kind {
                GitHistoryRefKind::Branch => 0,
                GitHistoryRefKind::Tag => 1,
                GitHistoryRefKind::Remote => 2,
            };
            order(left.kind)
                .cmp(&order(right.kind))
                .then_with(|| left.label.cmp(&right.label))
        });
    }
    refs_by_sha
}

/// Absolute form of a possibly-relative path (no filesystem access).
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// Folders under home that people keep projects in, tried for a bare name
/// after the folders beside the projects this device already has.
const PROJECT_ROOTS: &[&str] = &[
    "codebase",
    "code",
    "projects",
    "src",
    "dev",
    "repos",
    "Documents/GitHub",
    "GitHub",
];
/// Most candidates a registration lists.
const PROJECT_CANDIDATES_MAX: usize = 8;
/// System folders no project may be registered in, or below.
const SYSTEM_FOLDERS: &[&str] = &[
    "c:/windows",
    "c:/program files",
    "c:/program files (x86)",
    "c:/programdata",
    "/etc",
    "/usr",
    "/bin",
    "/sbin",
    "/boot",
    "/proc",
    "/sys",
    "/dev",
    "/system",
    "/library",
    "/applications",
];

/// A project this device already has: its folder and the name it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownFolder {
    pub path: String,
    pub name: Option<String>,
}

/// A folder a project query may mean.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectFolder {
    pub path: String,
    /// How it was found: `path`, `existing`, `sibling`, `drive`, `home` or
    /// `index`.
    pub source: &'static str,
    pub is_repo: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSearch {
    pub candidates: Vec<ProjectFolder>,
    /// Where it looked, for a "not found" message.
    pub searched: Vec<String>,
    /// The search could not finish: the home index was still building.
    pub incomplete: bool,
}

/// A path as a comparison key: one separator, no trailing one, lower case.
pub(crate) fn path_key(path: &Path) -> String {
    let key = path.to_string_lossy().replace('\\', "/");
    key.trim_end_matches('/').to_lowercase()
}

/// A query that names a place, not a project: it has a separator, starts
/// with `~` or a drive letter, or is absolute.
fn looks_like_path(query: &str) -> bool {
    let query = query.trim();
    let bytes = query.as_bytes();
    query.contains(['/', '\\'])
        || query.starts_with('~')
        || Path::new(query).is_absolute()
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// `~`, `~/x` and `~\x` against `home`; anything else as it is.
fn expand_query_path(query: &str, home: &Path) -> PathBuf {
    match query.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with(['/', '\\']) => {
            home.join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(query),
    }
}

/// Why `path` cannot be a project folder: the home folder itself, a drive or
/// filesystem root, or a system folder.
pub(crate) fn unsafe_project_root(path: &Path, home: &Path) -> Option<&'static str> {
    if path.parent().is_none() {
        return Some("a drive or filesystem root");
    }
    let key = path_key(path);
    if key == path_key(home) {
        return Some("the home folder");
    }
    SYSTEM_FOLDERS
        .iter()
        .any(|dir| key == *dir || key.starts_with(&format!("{dir}/")))
        .then_some("a system folder")
}

/// The folders `query` may mean on this device, the first tier that finds any:
/// a path (`~`, `~/x`, an absolute one); a project this device already has by
/// that name; a folder of that name beside a project it has (this is how
/// `t3code` is found next to `D:\zeron`); one at the top of a drive or in a
/// folder people keep projects in. Never creates anything and never guesses:
/// the caller registers only a single candidate. `Err` refuses a path that
/// cannot be a project.
pub(crate) fn find_project_folders(
    query: &str,
    known: &[KnownFolder],
    home: &Path,
    drives: &[PathBuf],
) -> Result<ProjectSearch, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("name the project folder: a path, or a short name like \"t3code\"".into());
    }
    let is_dir = |path: &Path| std::fs::metadata(path).is_ok_and(|meta| meta.is_dir());
    let mut search = ProjectSearch::default();
    let add = |search: &mut ProjectSearch, path: &Path, source: &'static str| {
        let key = path_key(path);
        if search
            .candidates
            .iter()
            .any(|c| path_key(Path::new(&c.path)) == key)
            || search.candidates.len() >= PROJECT_CANDIDATES_MAX
        {
            return;
        }
        search.candidates.push(ProjectFolder {
            path: folder_browser_path(path),
            source,
            is_repo: path.join(".git").exists(),
        });
    };
    // Where this device's projects sit.
    let mut parents: Vec<PathBuf> = Vec::new();
    for folder in known {
        if let Some(parent) = Path::new(&folder.path).parent()
            && !parents
                .iter()
                .any(|seen| path_key(seen) == path_key(parent))
        {
            parents.push(parent.to_path_buf());
        }
    }

    // 1. A path.
    if looks_like_path(query) {
        let given = expand_query_path(query, home);
        let places: Vec<PathBuf> = if given.is_absolute() {
            vec![given]
        } else {
            // Relative to home, then beside the projects it has.
            std::iter::once(home.join(&given))
                .chain(parents.iter().map(|parent| parent.join(&given)))
                .collect()
        };
        for place in places {
            search.searched.push(folder_browser_path(&place));
            if !is_dir(&place) {
                continue;
            }
            if let Some(why) = unsafe_project_root(&place, home) {
                return Err(format!(
                    "{} is {why}; register a project folder inside it instead",
                    folder_browser_path(&place)
                ));
            }
            add(&mut search, &place, "path");
        }
        return Ok(search);
    }

    // 2. A project it already has.
    let name = query.to_lowercase();
    let last = |path: &str| {
        path.trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_lowercase()
    };
    for folder in known {
        let named = folder
            .name
            .as_deref()
            .is_some_and(|given| given.trim().to_lowercase() == name);
        if named || last(&folder.path) == name {
            add(&mut search, Path::new(&folder.path), "existing");
        }
    }
    if !search.candidates.is_empty() {
        return Ok(search);
    }

    // 3. Beside the projects it has.
    for parent in &parents {
        search.searched.push(folder_browser_path(parent));
        if let Some(place) = child_folder(parent, query)
            && unsafe_project_root(&place, home).is_none()
        {
            add(&mut search, &place, "sibling");
        }
    }
    if !search.candidates.is_empty() {
        return Ok(search);
    }

    // 4. The top of home and of each drive, and the usual project folders.
    let mut roots: Vec<(PathBuf, &'static str)> = vec![(home.to_path_buf(), "home")];
    roots.extend(PROJECT_ROOTS.iter().map(|root| (home.join(root), "home")));
    roots.extend(drives.iter().map(|drive| (drive.clone(), "drive")));
    for (root, source) in roots {
        if !is_dir(&root) {
            continue;
        }
        search.searched.push(folder_browser_path(&root));
        if let Some(place) = child_folder(&root, query)
            && unsafe_project_root(&place, home).is_none()
        {
            add(&mut search, &place, source);
        }
    }
    Ok(search)
}

/// The folder in `dir` named `name`, ignoring case (as the name's own match
/// does), also on a file system that does not: an exact name first, else the
/// one entry whose name differs only in case.
fn child_folder(dir: &Path, name: &str) -> Option<PathBuf> {
    let is_dir = |path: &Path| std::fs::metadata(path).is_ok_and(|meta| meta.is_dir());
    let exact = dir.join(name);
    if is_dir(&exact) {
        return Some(exact);
    }
    let wanted = name.to_lowercase();
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| {
            entry.file_name().to_string_lossy().to_lowercase() == wanted
                && is_dir(&entry.path())
        })
        .map(|entry| entry.path())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_home_requires_an_explicit_usable_directory() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            session_home_dir_with(|name| (name == "HOME").then(|| home.path().as_os_str().into()))
                .unwrap(),
            home.path()
        );
        assert!(session_home_dir_with(|_| None).is_err());
        assert!(session_home_dir_with(|name| (name == "HOME").then(|| "relative".into())).is_err());
    }

    fn history_commit(sha: String, parent_sha: Option<String>) -> GitHistoryCommit {
        GitHistoryCommit {
            subject: sha.clone(),
            sha,
            parent_shas: parent_sha.into_iter().collect(),
            author_name: "Test".into(),
            author_email: "test@example.com".into(),
            authored_at: "2026-08-20T12:00:00Z".into(),
            refs: Vec::new(),
        }
    }

    #[test]
    fn expand_home_rewrites_tilde_and_leaves_other_paths() {
        let home = session_home_dir().unwrap();
        assert_eq!(expand_home("~").unwrap(), home.to_string_lossy());
        assert_eq!(
            expand_home("~/proj").unwrap(),
            home.join("proj").to_string_lossy()
        );
        assert_eq!(expand_home("/abs/path").unwrap(), "/abs/path");
        assert_eq!(expand_home("~nope").unwrap(), "~nope");
    }

    fn make_folders(root: &Path, names: &[&str]) {
        for name in names {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
    }

    fn known(path: &Path, name: Option<&str>) -> KnownFolder {
        KnownFolder {
            path: path.to_string_lossy().into_owned(),
            name: name.map(str::to_owned),
        }
    }

    #[test]
    fn a_short_name_is_found_beside_a_project_the_device_already_has() {
        // The layout of a machine whose projects sit at a drive's top.
        let drive = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        make_folders(drive.path(), &["zeron", "t3code"]);
        let have = [known(&drive.path().join("zeron"), None)];
        let found = find_project_folders("t3code", &have, home.path(), &[]).unwrap();
        assert_eq!(found.candidates.len(), 1, "{found:?}");
        assert_eq!(found.candidates[0].source, "sibling");
        assert!(Path::new(&found.candidates[0].path).ends_with("t3code"));
        assert!(!found.candidates[0].is_repo);
        // The match ignores case.
        let found = find_project_folders("T3Code", &have, home.path(), &[]).unwrap();
        assert_eq!(found.candidates.len(), 1, "{found:?}");
    }

    #[test]
    fn a_short_name_is_found_in_the_usual_project_folders_and_drive_tops() {
        let home = tempfile::tempdir().unwrap();
        let drive = tempfile::tempdir().unwrap();
        make_folders(
            home.path(),
            &["codebase/alpha", "beta", "Documents/GitHub/gamma"],
        );
        make_folders(drive.path(), &["delta"]);
        let drives = [drive.path().to_path_buf()];
        for (name, source) in [
            ("alpha", "home"),
            ("beta", "home"),
            ("gamma", "home"),
            ("delta", "drive"),
        ] {
            let found = find_project_folders(name, &[], home.path(), &drives).unwrap();
            assert_eq!(found.candidates.len(), 1, "{name}: {found:?}");
            assert_eq!(found.candidates[0].source, source, "{name}");
        }
        let none = find_project_folders("missing", &[], home.path(), &drives).unwrap();
        assert!(none.candidates.is_empty());
        assert!(!none.searched.is_empty(), "it says where it looked");
    }

    #[test]
    fn a_project_the_device_has_wins_and_is_not_found_twice() {
        let home = tempfile::tempdir().unwrap();
        let drive = tempfile::tempdir().unwrap();
        make_folders(drive.path(), &["zeron", "t3code"]);
        make_folders(home.path(), &["codebase/t3code"]);
        let have = [
            known(&drive.path().join("t3code"), None),
            known(&drive.path().join("zeron"), Some("Pi client")),
        ];
        let by_folder = find_project_folders("t3code", &have, home.path(), &[]).unwrap();
        assert_eq!(by_folder.candidates.len(), 1, "{by_folder:?}");
        assert_eq!(by_folder.candidates[0].source, "existing");
        let by_name = find_project_folders("pi client", &have, home.path(), &[]).unwrap();
        assert_eq!(by_name.candidates.len(), 1, "{by_name:?}");
        assert!(Path::new(&by_name.candidates[0].path).ends_with("zeron"));
    }

    #[test]
    fn two_folders_of_one_name_in_a_tier_are_both_offered() {
        let home = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        make_folders(a.path(), &["app"]);
        make_folders(b.path(), &["app"]);
        let drives = [a.path().to_path_buf(), b.path().to_path_buf()];
        let found = find_project_folders("app", &[], home.path(), &drives).unwrap();
        assert_eq!(found.candidates.len(), 2, "{found:?}");
    }

    #[test]
    fn a_path_query_expands_home_with_either_separator_and_never_falls_back_to_a_name() {
        let home = tempfile::tempdir().unwrap();
        make_folders(home.path(), &["proj", "codebase/t3code"]);
        for query in ["~/proj", "~\\proj"] {
            let found = find_project_folders(query, &[], home.path(), &[]).unwrap();
            assert_eq!(found.candidates.len(), 1, "{query}: {found:?}");
            assert_eq!(found.candidates[0].source, "path");
            assert!(Path::new(&found.candidates[0].path).ends_with("proj"));
        }
        let absolute = home.path().join("proj");
        let found =
            find_project_folders(&absolute.to_string_lossy(), &[], home.path(), &[]).unwrap();
        assert_eq!(found.candidates.len(), 1);
        // A path that is not a folder finds nothing, even though a folder
        // of the last name exists elsewhere.
        let missing = find_project_folders("~/elsewhere/t3code", &[], home.path(), &[]).unwrap();
        assert!(missing.candidates.is_empty(), "{missing:?}");
        assert_eq!(missing.searched.len(), 1);
    }

    #[test]
    fn home_a_root_and_system_folders_are_refused() {
        let home = tempfile::tempdir().unwrap();
        assert!(find_project_folders("~", &[], home.path(), &[]).is_err());
        assert!(
            find_project_folders(&home.path().to_string_lossy(), &[], home.path(), &[]).is_err()
        );
        assert_eq!(
            unsafe_project_root(Path::new("/"), home.path()),
            Some("a drive or filesystem root")
        );
        for system in ["/etc", "/usr/local", "C:\\Windows", "c:/Program Files/App"] {
            assert_eq!(
                unsafe_project_root(Path::new(system), home.path()),
                Some("a system folder"),
                "{system}"
            );
        }
        assert_eq!(
            unsafe_project_root(&home.path().join("proj"), home.path()),
            None
        );
        // A name that is a system folder is skipped, not registered.
        make_folders(home.path(), &["etc"]);
        let found = find_project_folders("etc", &[], home.path(), &[]).unwrap();
        assert_eq!(
            found.candidates.len(),
            1,
            "a folder named etc under home is fine"
        );
        assert!(find_project_folders(" ", &[], home.path(), &[]).is_err());
    }

    #[test]
    fn a_folder_with_git_metadata_reads_as_a_repository() {
        let home = tempfile::tempdir().unwrap();
        make_folders(home.path(), &["codebase/plain", "codebase/repo/.git"]);
        std::fs::create_dir_all(home.path().join("codebase/linked")).unwrap();
        // A linked worktree's `.git` is a file.
        std::fs::write(home.path().join("codebase/linked/.git"), "gitdir: x").unwrap();
        let is_repo = |name: &str| {
            find_project_folders(name, &[], home.path(), &[])
                .unwrap()
                .candidates[0]
                .is_repo
        };
        assert!(is_repo("repo"));
        assert!(is_repo("linked"));
        assert!(!is_repo("plain"));
    }

    #[test]
    fn only_a_place_reads_as_a_path() {
        for path in ["~", "~/x", "a/b", "a\\b", "D:\\x", "D:x", "/abs"] {
            assert!(looks_like_path(path), "{path}");
        }
        for name in ["t3code", "my-app", "app.v2"] {
            assert!(!looks_like_path(name), "{name}");
        }
    }

    #[test]
    fn parses_common_github_remote_forms() {
        let expected = Some(("openai".to_string(), "codex".to_string()));
        assert_eq!(
            parse_github_remote("https://github.com/openai/codex.git"),
            expected
        );
        assert_eq!(
            parse_github_remote("git@github.com:openai/codex.git"),
            expected
        );
        assert_eq!(parse_github_remote("https://gitlab.com/openai/codex"), None);
    }

    #[tokio::test]
    async fn github_avatar_downloads_once_into_the_local_cache() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let bytes = b"\xff\xd8\xffavatar".to_vec();
        let served = bytes.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        served.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(&served).await.unwrap();
        });

        let data = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));
        let url = format!("http://{address}/avatar.jpg");
        let first = repos.cache_github_avatar(&url).await.expect("downloaded");
        server.await.unwrap();
        assert_eq!(tokio::fs::read(&first).await.unwrap(), bytes);
        assert_eq!(
            repos.cache_github_avatar(&url).await.as_deref(),
            Some(first.as_str())
        );
    }

    #[test]
    fn linux_drives_take_media_and_mnt_mounts_system_first() {
        let mounts = "\
sysfs /sys sysfs rw 0 0
/dev/nvme0n1p2 / ext4 rw 0 0
tmpfs /run tmpfs rw 0 0
/dev/sda1 /media/wing/T7\\040Shield exfat rw 0 0
/dev/sdb1 /run/media/wing/Backup ext4 rw 0 0
/dev/sdc1 /mnt/scratch ext4 rw 0 0
/dev/nvme0n1p2 /var/lib/docker ext4 rw 0 0
";
        let drives = finish_drives(linux_drives(mounts));
        let rows: Vec<(&str, &str)> = drives
            .iter()
            .map(|d| (d.name.as_str(), d.path.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("System", "/"),
                ("Backup", "/run/media/wing/Backup"),
                ("scratch", "/mnt/scratch"),
                ("T7 Shield", "/media/wing/T7 Shield"),
            ]
        );
    }

    #[test]
    fn linux_drives_take_custom_top_level_block_mounts_not_system_splits() {
        let mounts = "\
/dev/nvme0n1p2 / ext4 rw 0 0
/dev/sdb1 /disk2 ext4 rw 0 0
/dev/mapper/vault /tank btrfs rw 0 0
/dev/nvme0n1p1 /boot/efi vfat rw 0 0
/dev/sdd1 /boot ext4 rw 0 0
/dev/sde1 /home ext4 rw 0 0
/dev/sdf1 /data/disks/a ext4 rw 0 0
/dev/loop3 /snap/core22/1234 squashfs ro 0 0
/dev/loop9 /disk3 ext4 rw 0 0
";
        let drives = finish_drives(linux_drives(mounts));
        let rows: Vec<&str> = drives.iter().map(|d| d.path.as_str()).collect();
        // /disk2 and /tank are user drive roots; a loop-mounted ext4 image at
        // a custom root counts too (it's squashfs snaps that are noise). The
        // system's own split partitions and deep mounts stay out.
        assert_eq!(rows, vec!["/", "/disk2", "/disk3", "/tank"]);
    }

    #[test]
    fn finish_drives_dedupes_by_mount_point_keeping_the_first_name() {
        let drives = finish_drives(vec![
            DriveEntry {
                name: "Macintosh HD".into(),
                path: "/".into(),
            },
            DriveEntry {
                name: "System".into(),
                path: "/".into(),
            },
        ]);
        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0].name, "Macintosh HD");
    }

    #[test]
    fn mount_point_unescape_handles_octal_and_lone_backslash() {
        assert_eq!(unescape_mount_point("/media/a\\040b"), "/media/a b");
        assert_eq!(unescape_mount_point("/media/tab\\011x"), "/media/tab\tx");
        assert_eq!(unescape_mount_point("/media/plain"), "/media/plain");
        assert_eq!(unescape_mount_point("/media/tail\\"), "/media/tail\\");
    }

    #[tokio::test]
    async fn list_drives_resolves_on_this_platform() {
        let data = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));
        // Content is machine-dependent; the call must succeed and dedupe.
        let drives = repos.list_drives().await.unwrap();
        let mut paths: Vec<&String> = drives.iter().map(|d| &d.path).collect();
        paths.dedup();
        assert_eq!(paths.len(), drives.len());
    }

    #[test]
    fn git_history_matches_unicode_case_insensitively() {
        let mut candidate = history_commit("a1b2c3d4".into(), None);
        candidate.subject = "RÉPARER la recherche".into();

        assert!(git_history_matches("réparer", &candidate));
    }

    #[test]
    fn history_compaction_handles_a_twenty_thousand_commit_gap() {
        const DEPTH: usize = 20_000;
        let commits = (0..DEPTH)
            .rev()
            .map(|index| {
                history_commit(
                    format!("c{index:05}"),
                    (index > 0).then(|| format!("c{:05}", index - 1)),
                )
            })
            .collect::<Vec<_>>();
        let newest = format!("c{:05}", DEPTH - 1);
        let oldest = "c00000".to_string();
        let visible = HashSet::from([newest.clone(), oldest.clone()]);

        let compact = compact_history_commits(&commits, &visible);

        assert_eq!(compact.len(), 2);
        assert_eq!(compact[0].sha, newest);
        assert_eq!(compact[0].parent_shas, vec![oldest.clone()]);
        assert_eq!(compact[1].sha, oldest);
        assert!(compact[1].parent_shas.is_empty());
    }

    #[test]
    fn home_folder_search_finds_nested_repositories() {
        let home = tempfile::tempdir().unwrap();
        for dir in [
            "work/clients/comet/.git",
            "work/clients/comet/src",
            "notes/comet-ideas",
            "notes/other",
        ] {
            std::fs::create_dir_all(home.path().join(dir)).unwrap();
        }
        std::fs::write(home.path().join("notes/comet-ideas/todo.md"), b"").unwrap();
        let home = std::fs::canonicalize(home.path()).unwrap();
        let search = WorkspaceSearch::new();
        // The first call starts the index; wait out its scan.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let result = loop {
            let result = search_folders_under(&search, &home, "comet", 20).unwrap();
            if !result.indexing && !result.matches.is_empty() {
                break result;
            }
            assert!(std::time::Instant::now() < deadline, "index never finished");
            std::thread::sleep(Duration::from_millis(20));
        };
        let found: Vec<_> = result
            .matches
            .iter()
            .map(|m| (m.relative.as_str(), m.is_repo))
            .collect();
        // The exact name outranks the shallower prefix match.
        assert_eq!(
            found,
            [("work/clients/comet", true), ("notes/comet-ideas", false)]
        );
        assert_eq!(
            result.matches[0].path,
            folder_browser_path(&home.join("work/clients/comet"))
        );
        assert!(
            search_folders_under(&search, &home, "", 20)
                .unwrap()
                .matches
                .is_empty()
        );
    }

    #[test]
    #[cfg(windows)]
    fn home_folder_search_accepts_both_windows_separators() {
        let home = tempfile::tempdir().unwrap();
        for dir in ["aa/bb/cc/dd/src", "aa/bb/cc/dd-other", "elsewhere/dd"] {
            std::fs::create_dir_all(home.path().join(dir)).unwrap();
        }
        let home = std::fs::canonicalize(home.path()).unwrap();
        let search = WorkspaceSearch::new();
        search.warm(&home).unwrap();
        assert!(search.wait_until_indexed(&home, Duration::from_secs(20)));

        let forward = search_folders_under(&search, &home, "aa/bb/cc/dd", 20).unwrap();
        assert_eq!(forward.matches[0].relative, "aa/bb/cc/dd");
        assert!(
            forward
                .matches
                .iter()
                .any(|m| m.relative == "aa/bb/cc/dd-other")
        );
        for query in [r"aa\bb\cc\dd", r"aa\bb/cc\dd", r"aa\bb\cc\dd\"] {
            let found = search_folders_under(&search, &home, query, 20).unwrap();
            assert_eq!(found.matches, forward.matches);
        }
    }

    #[test]
    #[cfg(not(windows))]
    fn home_folder_search_preserves_literal_posix_backslashes() {
        let home = tempfile::tempdir().unwrap();
        let literal = r"aa\bb\cc\dd";
        for dir in [literal, "aa/bb/cc/dd"] {
            std::fs::create_dir_all(home.path().join(dir)).unwrap();
        }
        let home = std::fs::canonicalize(home.path()).unwrap();
        let search = WorkspaceSearch::new();
        search.warm(&home).unwrap();
        assert!(search.wait_until_indexed(&home, Duration::from_secs(20)));

        let found = search_folders_under(&search, &home, literal, 20).unwrap();
        assert_eq!(found.matches.len(), 1);
        assert_eq!(found.matches[0].relative, literal);
        let forward = search_folders_under(&search, &home, "aa/bb/cc/dd", 20).unwrap();
        assert_eq!(forward.matches.len(), 1);
        assert_eq!(forward.matches[0].relative, "aa/bb/cc/dd");
    }

    #[test]
    #[cfg(not(windows))]
    fn browser_paths_preserve_native_posix_spelling() {
        for path in [
            "/home/ana/code",
            "/Users/Ana/code",
            r"/tmp/name\with\backslashes",
        ] {
            assert_eq!(folder_browser_path(Path::new(path)), path);
        }
    }

    #[test]
    #[cfg(windows)]
    fn browser_paths_simplify_only_safe_windows_drive_paths() {
        assert_eq!(
            folder_browser_path(Path::new(r"\\?\C:\Users\Ana\repo")),
            r"C:\Users\Ana\repo"
        );
        for path in [
            r"\\?\C:\repo.",
            r"\\?\C:\NUL.txt",
            r"\\?\UNC\server\share\repo",
        ] {
            assert_eq!(folder_browser_path(Path::new(path)), path);
        }
        let long = format!(r"\\?\C:\{}\{}", "a".repeat(150), "b".repeat(150));
        assert_eq!(folder_browser_path(Path::new(&long)), long);
    }

    fn ranked(query: &str, candidates: &[(&str, bool)]) -> Vec<String> {
        let candidates = candidates
            .iter()
            .map(|(path, is_repo)| ((*path).to_owned(), *is_repo))
            .collect();
        rank_home_folders(query, candidates)
            .into_iter()
            .map(|(path, _)| path)
            .collect()
    }

    #[test]
    fn home_folders_rank_by_their_own_name() {
        let candidates = [
            ("Drive/notes about comet launch", false),
            ("dev/comet/apps/ios", false),
            ("old/comet-archive", false),
            ("dev/comet", false),
            ("dev/mycomet", false),
        ];
        assert_eq!(
            ranked("Comet", &candidates),
            [
                "dev/comet",
                "old/comet-archive",
                // Same tier, repo flag and depth: the index's order decides.
                "Drive/notes about comet launch",
                "dev/mycomet",
            ]
        );
    }

    #[test]
    fn home_folders_put_repositories_first_within_a_tier() {
        let candidates = [("a/comet-docs", false), ("b/c/comet-app", true)];
        assert_eq!(
            ranked("comet", &candidates),
            ["b/c/comet-app", "a/comet-docs"]
        );
        // Typo matches survive only when nothing contains the query, and
        // order the same way.
        let candidates = [
            ("Models/Comfy/comfy_types", false),
            ("dev/comet", true),
            ("dev/tools", false),
        ];
        assert_eq!(
            ranked("comt", &candidates),
            ["dev/comet", "dev/tools", "Models/Comfy/comfy_types"]
        );
    }

    #[test]
    fn slash_queries_match_trailing_segment_prefixes() {
        let candidates = [
            ("Documentos/development/comet/apps", false),
            ("Documentos/development/comet", true),
            ("Documentos/dev/tools", false),
            ("dev/com", false),
        ];
        assert_eq!(
            ranked("dev/com", &candidates),
            ["dev/com", "Documentos/development/comet"]
        );
    }

    fn search_files_blocking(
        root: &Path,
        query: &str,
        featured_paths: &[String],
    ) -> Result<Vec<FileSearchMatch>, EngineError> {
        super::search_files_blocking(&WorkspaceSearch::new(), root, query, featured_paths)
    }

    #[test]
    fn search_files_obeys_gitignore_and_returns_directories() {
        let root = tempfile::tempdir().unwrap();
        // Dotfiles are indexed only inside a real repository.
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root.path())
            .status()
            .unwrap();
        assert!(init.success());
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/composer.rs"), "").unwrap();
        std::fs::write(root.path().join(".secret"), "").unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored\n").unwrap();
        std::fs::create_dir(root.path().join("ignored")).unwrap();
        std::fs::write(root.path().join("ignored/nope.rs"), "").unwrap();

        let matches = search_files_blocking(root.path(), "src", &[]).unwrap();
        assert!(
            matches
                .iter()
                .any(|entry| entry.path == "src" && entry.is_dir)
        );
        assert!(matches.iter().any(|entry| entry.path == "src/composer.rs"));
        assert!(
            !matches
                .iter()
                .any(|entry| entry.path.starts_with("ignored"))
        );
        assert!(
            search_files_blocking(root.path(), "secret", &[])
                .unwrap()
                .iter()
                .any(|entry| entry.path == ".secret")
        );
        assert!(!matches.iter().any(|entry| entry.path.starts_with(".git/")));
    }

    #[test]
    fn empty_search_features_recent_paths_before_shallow_entries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("README.md"), "").unwrap();
        std::fs::write(root.path().join("src/deep.rs"), "").unwrap();

        let matches = search_files_blocking(root.path(), "", &["src/deep.rs".into()]).unwrap();
        assert_eq!(
            matches.first().map(|entry| entry.path.as_str()),
            Some("src/deep.rs")
        );
        assert!(matches.iter().any(|entry| entry.path == "README.md"));
    }

    #[test]
    fn search_files_prefers_filename_matches() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("composer/docs")).unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("composer/docs/readme.md"), "").unwrap();
        std::fs::write(root.path().join("src/composer.rs"), "").unwrap();

        let matches = search_files_blocking(root.path(), "composer", &[]).unwrap();
        let composer = matches
            .iter()
            .position(|entry| entry.path == "src/composer.rs")
            .unwrap();
        let path_only = matches
            .iter()
            .position(|entry| entry.path == "composer/docs/readme.md")
            .unwrap();
        assert!(composer < path_only);
    }

    #[tokio::test]
    async fn workspace_checkout_rejects_sibling_paths() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let sibling = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));

        assert_eq!(
            repos.workspace_checkout(root.path(), root.path()).await,
            std::fs::canonicalize(root.path()).ok()
        );
        assert!(
            repos
                .workspace_checkout(root.path(), sibling.path())
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn concurrent_searches_of_one_checkout_do_not_cancel_each_other() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("alpha.rs"), "").unwrap();
        std::fs::write(root.path().join("beta.rs"), "").unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));

        let alpha = repos.search_files(root.path().into(), "alpha".into(), Vec::new());
        let beta = repos.search_files(root.path().into(), "beta".into(), Vec::new());
        let (alpha, beta) = tokio::join!(alpha, beta);

        assert_eq!(alpha.unwrap()[0].path, "alpha.rs");
        assert_eq!(beta.unwrap()[0].path, "beta.rs");
    }

    mod merge_preview {
        use super::*;

        fn sh(dir: &Path, args: &[&str]) -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        fn commit_file(dir: &Path, name: &str, body: &str) {
            std::fs::write(dir.join(name), body).unwrap();
            sh(dir, &["add", "."]);
            sh(dir, &["commit", "-q", "-m", name]);
        }

        struct Fixture {
            _data: tempfile::TempDir,
            lead: PathBuf,
            wt: PathBuf,
            repos: Repos,
        }

        /// A repo on `main` with a three-line a.txt, plus a `zeron/*` worktree.
        async fn fixture() -> Fixture {
            let data = tempfile::tempdir().unwrap();
            let lead = data.path().join("lead");
            std::fs::create_dir_all(&lead).unwrap();
            sh(&lead, &["init", "-q", "-b", "main"]);
            commit_file(&lead, "a.txt", "one\ntwo\nthree\n");
            commit_file(&lead, "b.txt", "b\n");
            let repos =
                Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));
            let wt = repos.create_worktree(&lead, "main").await.unwrap();
            Fixture {
                lead,
                wt: PathBuf::from(wt.path),
                repos,
                _data: data,
            }
        }

        /// HEAD, status and refs of both trees: must not change in a preview.
        fn snapshot(f: &Fixture) -> Vec<String> {
            [&f.lead, &f.wt]
                .iter()
                .flat_map(|d| {
                    [
                        sh(d, &["rev-parse", "HEAD"]),
                        sh(d, &["status", "--porcelain"]),
                        sh(d, &["for-each-ref"]),
                        sh(d, &["ls-files", "-u"]),
                    ]
                })
                .collect()
        }

        #[tokio::test]
        async fn a_clean_branch_is_a_fast_forward_that_adds_a_file() {
            let f = fixture().await;
            commit_file(&f.wt, "new.txt", "hi\n");
            let before = snapshot(&f);
            let p = f.repos.merge_preview(&f.wt, &f.lead, None).await.unwrap();
            assert_eq!(p.conflict_check, MergeConflictCheck::Clean);
            assert_eq!((p.ahead, p.behind), (1, 0));
            assert!(p.fast_forward && !p.already_merged);
            assert_eq!(p.base, "main");
            assert!(p.branch.starts_with("zeron/"));
            assert_eq!(
                p.files,
                vec![MergePreviewFile {
                    path: "new.txt".into(),
                    status: "A".into(),
                    old_path: None
                }]
            );
            assert_eq!(p.merge_tree.as_deref().map(str::len), Some(40));
            assert!(p.git_version.is_some());
            assert_eq!(snapshot(&f), before, "a preview changes nothing");
        }

        #[tokio::test]
        async fn a_lead_that_moved_on_is_behind_but_still_clean() {
            let f = fixture().await;
            commit_file(&f.wt, "new.txt", "hi\n");
            commit_file(&f.lead, "b.txt", "b2\n");
            let p = f.repos.merge_preview(&f.wt, &f.lead, None).await.unwrap();
            assert_eq!((p.ahead, p.behind), (1, 1));
            assert!(!p.fast_forward);
            assert_eq!(p.conflict_check, MergeConflictCheck::Clean);
            assert!(p.conflicts.is_empty());
        }

        #[tokio::test]
        async fn editing_the_same_line_conflicts() {
            let f = fixture().await;
            commit_file(&f.wt, "a.txt", "one\nmember\nthree\n");
            commit_file(&f.lead, "a.txt", "one\nlead\nthree\n");
            let before = snapshot(&f);
            let p = f.repos.merge_preview(&f.wt, &f.lead, None).await.unwrap();
            assert_eq!(p.conflict_check, MergeConflictCheck::Conflicts);
            assert_eq!(p.conflicts, vec!["a.txt".to_string()]);
            assert_eq!(p.conflicts_total, 1);
            let tree = p.merge_tree.unwrap();
            assert!(tree.len() == 40 && tree.chars().all(|c| c.is_ascii_hexdigit()));
            assert_eq!(snapshot(&f), before);
            assert_eq!(sh(&f.lead, &["ls-files", "-u"]), "");
        }

        #[tokio::test]
        async fn uncommitted_edits_are_counted_apart_from_the_merge() {
            let f = fixture().await;
            commit_file(&f.wt, "new.txt", "hi\n");
            std::fs::write(f.wt.join("untracked.txt"), "u\n").unwrap();
            std::fs::write(f.wt.join("b.txt"), "changed\n").unwrap();
            std::fs::write(f.wt.join("staged.txt"), "s\n").unwrap();
            sh(&f.wt, &["add", "staged.txt"]);
            let p = f.repos.merge_preview(&f.wt, &f.lead, None).await.unwrap();
            let d = &p.uncommitted;
            assert_eq!((d.staged, d.unstaged, d.untracked, d.total), (1, 1, 1, 3));
            assert_eq!(d.paths.len(), 3);
            assert_eq!(p.files.len(), 1, "uncommitted files are not in the merge");
        }

        #[tokio::test]
        async fn an_unknown_base_is_an_error_not_a_conflict() {
            let f = fixture().await;
            let err = f
                .repos
                .merge_preview(&f.wt, &f.lead, Some("no-such-branch"))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("no-such-branch"), "{err}");
            let err = f
                .repos
                .merge_preview(&f.wt, &f.lead, Some("--output=x"))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("not found"), "{err}");
        }

        #[tokio::test]
        async fn unrelated_histories_fail_with_a_note() {
            let f = fixture().await;
            sh(&f.wt, &["checkout", "-q", "--orphan", "island"]);
            sh(&f.wt, &["rm", "-rq", "--cached", "."]);
            commit_file(&f.wt, "island.txt", "x\n");
            let p = f.repos.merge_preview(&f.wt, &f.lead, None).await.unwrap();
            assert_eq!(p.conflict_check, MergeConflictCheck::Failed);
            assert!(p.note.unwrap().contains("no history"));
            assert_eq!(p.branch, "island");
        }

        #[tokio::test]
        async fn a_merged_branch_reports_already_merged() {
            let f = fixture().await;
            let p = f.repos.merge_preview(&f.wt, &f.lead, None).await.unwrap();
            assert!(p.already_merged && !p.fast_forward);
            assert_eq!((p.ahead, p.behind), (0, 0));
            assert!(p.files.is_empty());
        }

        #[tokio::test]
        async fn a_detached_member_still_previews() {
            let f = fixture().await;
            commit_file(&f.wt, "new.txt", "hi\n");
            sh(&f.wt, &["checkout", "-q", "--detach"]);
            let p = f.repos.merge_preview(&f.wt, &f.lead, None).await.unwrap();
            assert_eq!(p.branch, p.branch_head[..7]);
            assert_eq!(p.ahead, 1);
            assert_eq!(p.conflict_check, MergeConflictCheck::Clean);
        }

        #[tokio::test]
        async fn an_explicit_base_and_a_missing_worktree() {
            let f = fixture().await;
            sh(&f.lead, &["branch", "release"]);
            commit_file(&f.wt, "new.txt", "hi\n");
            let p = f
                .repos
                .merge_preview(&f.wt, &f.lead, Some("release"))
                .await
                .unwrap();
            assert_eq!(p.base, "release");
            let err = f
                .repos
                .merge_preview(&f.lead.join("nope"), &f.lead, None)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("no worktree"), "{err}");
        }

        #[tokio::test]
        async fn without_merge_tree_support_everything_else_is_still_reported() {
            let f = fixture().await;
            commit_file(&f.wt, "new.txt", "hi\n");
            std::fs::write(f.wt.join("dirty.txt"), "d\n").unwrap();
            let p = f
                .repos
                .merge_preview_with(&f.wt, &f.lead, None, false, Some("2.30.0".into()))
                .await
                .unwrap();
            assert_eq!(p.conflict_check, MergeConflictCheck::Unavailable);
            assert!(p.note.unwrap().contains("2.38"));
            assert!(p.conflicts.is_empty() && p.merge_tree.is_none());
            assert_eq!((p.ahead, p.behind, p.files.len()), (1, 0, 1));
            assert_eq!(p.uncommitted.untracked, 1);
        }

        #[test]
        fn git_versions_parse_across_platforms() {
            let v = |s| parse_git_version(s).map(|(a, b, _)| (a, b));
            assert_eq!(v("git version 2.43.0\n"), Some((2, 43)));
            assert_eq!(v("git version 2.39.5 (Apple Git-154)"), Some((2, 39)));
            assert_eq!(v("git version 2.37.1.windows.1"), Some((2, 37)));
            assert_eq!(
                parse_git_version("git version 2.37.1.windows.1").unwrap().2,
                "2.37.1.windows.1"
            );
            assert_eq!(v("not git"), None);
        }

        #[test]
        fn name_status_and_porcelain_records_parse() {
            let files = parse_name_status("M\0a.txt\0R100\0old.txt\0new.txt\0A\0c.txt\0");
            assert_eq!(files.len(), 3);
            assert_eq!(files[1].path, "new.txt");
            assert_eq!(files[1].old_path.as_deref(), Some("old.txt"));
            assert_eq!(files[1].status, "R");
            let d =
                parse_porcelain_status("R  to.txt\0from.txt\0 M m.txt\0?? u.txt\0MM both.txt\0");
            assert_eq!((d.staged, d.unstaged, d.untracked, d.total), (2, 2, 1, 4));
            assert_eq!(d.paths[0], "to.txt");
        }
    }
}
