//! `MergePreview` over the in-memory RPC transport: only an orchestrator root
//! previews its own tree's worktree members, and nothing on disk changes.

use std::path::Path;
use std::sync::Arc;

use zeron_engine::{EngineCore, HarnessRegistry, Repos};
use zeron_rpc::methods;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .expect("git spawns");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Everything a preview must leave alone, for both trees.
fn fingerprint(dirs: &[&Path]) -> Vec<String> {
    dirs.iter()
        .flat_map(|dir| {
            [
                git(dir, &["rev-parse", "HEAD"]),
                git(dir, &["status", "--porcelain"]),
                git(dir, &["for-each-ref"]),
                git(dir, &["ls-files", "-u"]),
            ]
        })
        .collect()
}

#[tokio::test]
async fn an_orchestrator_root_previews_its_members_worktree_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let repo_path = repo.to_string_lossy().to_string();

    let repos = Repos::with_worktrees_root(
        &tmp.path().join("repos-data"),
        "device",
        tmp.path().join("worktrees"),
    );
    let worktree = repos.create_worktree(&repo, "main").await.unwrap();
    let wt = Path::new(&worktree.path);
    std::fs::write(wt.join("new.txt"), "hi\n").unwrap();
    git(wt, &["add", "."]);
    git(wt, &["commit", "-q", "-m", "member work"]);
    std::fs::write(wt.join("scratch.txt"), "wip\n").unwrap();

    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let core = EngineCore::assemble(
        &data,
        Arc::new(HarnessRegistry::new()),
        zeron_proto::HarnessId::Mock,
        None,
    )
    .expect("engine assembles");
    let client = zeron_rpc::memory_client(core.rpc_service());
    core.workspace
        .create_space("space-1", &core.device_id, &repo_path, None, true)
        .expect("space");
    let chat = |id: &str, parent: Option<&str>, cwd: Option<&str>| {
        core.workspace
            .create_chat_with_parent(
                id,
                Some("space-1"),
                None,
                None,
                cwd.map(str::to_owned),
                parent.map(str::to_owned),
            )
            .expect("chat");
    };
    chat("orchestrator-a", None, None);
    chat("writer", Some("orchestrator-a"), Some(&worktree.path));
    // A read-only member runs in the project folder itself.
    chat("reader", Some("orchestrator-a"), None);
    chat("orchestrator-b", None, None);
    chat("other-writer", Some("orchestrator-b"), Some(&worktree.path));

    let before = fingerprint(&[&repo, wt]);
    let preview = client
        .call(
            methods::MERGE_PREVIEW,
            serde_json::json!({ "chatId": "orchestrator-a", "member": "writer" }),
        )
        .await
        .expect("MergePreview");
    assert_eq!(preview["chatId"], "writer");
    assert_eq!(preview["base"], "main");
    assert_eq!(preview["branch"], worktree.branch);
    assert_eq!(preview["ahead"], 1);
    assert_eq!(preview["behind"], 0);
    assert_eq!(preview["fastForward"], true);
    assert_eq!(preview["conflictCheck"], "clean");
    assert_eq!(preview["files"][0]["path"], "new.txt");
    assert_eq!(preview["files"][0]["status"], "A");
    assert_eq!(preview["uncommitted"]["untracked"], 1);
    assert_eq!(fingerprint(&[&repo, wt]), before, "the trees are untouched");

    let explicit = client
        .call(
            methods::MERGE_PREVIEW,
            serde_json::json!({ "chatId": "orchestrator-a", "member": "writer", "base": "main" }),
        )
        .await
        .expect("MergePreview with a base");
    assert_eq!(explicit["baseHead"], preview["baseHead"]);

    let refused = |params: serde_json::Value| {
        let client = &client;
        async move {
            client
                .call(methods::MERGE_PREVIEW, params)
                .await
                .expect_err("refused")
                .to_string()
        }
    };
    let by_member = refused(serde_json::json!({ "chatId": "writer", "member": "writer" })).await;
    assert!(by_member.contains("Only the orchestrator"), "{by_member}");
    let other_tree =
        refused(serde_json::json!({ "chatId": "orchestrator-a", "member": "other-writer" })).await;
    assert!(other_tree.contains("not a session"), "{other_tree}");
    let unknown =
        refused(serde_json::json!({ "chatId": "orchestrator-a", "member": "nope" })).await;
    assert!(unknown.contains("not a session"), "{unknown}");
    let read_only =
        refused(serde_json::json!({ "chatId": "orchestrator-a", "member": "reader" })).await;
    assert!(read_only.contains("no worktree"), "{read_only}");
    let no_base = refused(
        serde_json::json!({ "chatId": "orchestrator-a", "member": "writer", "base": "gone" }),
    )
    .await;
    assert!(no_base.contains("gone"), "{no_base}");
    assert_eq!(fingerprint(&[&repo, wt]), before);
    core.shutdown().await;
}
