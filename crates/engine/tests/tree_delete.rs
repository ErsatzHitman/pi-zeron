//! Deleting a chat takes everything below it with it, whatever the chat is:
//! an orchestrator's root, a member's branch, the side chats of any chat.

use std::sync::Arc;

use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_proto::HarnessId;
use zeron_rpc::methods;

fn create(core: &EngineCore, id: &str, parent: Option<&str>) {
    core.workspace
        .create_chat_with_parent(
            id,
            None,
            Some(&core.device_id),
            None,
            Some("/tmp".into()),
            parent.map(str::to_owned),
        )
        .unwrap();
}

fn exists(core: &EngineCore, id: &str) -> bool {
    core.workspace.chat(id).unwrap().is_some()
}

async fn delete(core: &EngineCore, id: &str) {
    zeron_rpc::memory_client(core.rpc_service())
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "deleteChat", "chatId": id }),
        )
        .await
        .expect("delete chat");
}

#[tokio::test]
async fn deleting_a_member_takes_its_branch_and_its_side_chats() {
    let dir = tempfile::tempdir().unwrap();
    let core = EngineCore::assemble(
        dir.path(),
        Arc::new(HarnessRegistry::new()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    create(&core, "orchestrator-a", None);
    create(&core, "m1", Some("orchestrator-a"));
    create(&core, "m2", Some("m1"));
    create(&core, "btw-aside", Some("m2"));
    create(&core, "plain", None);
    create(&core, "side", Some("plain"));

    // A member is no different from a root here: its branch goes with it.
    delete(&core, "m1").await;
    for gone in ["m1", "m2", "btw-aside"] {
        assert!(!exists(&core, gone), "{gone} outlived its ancestor");
    }
    for kept in ["orchestrator-a", "plain", "side"] {
        assert!(exists(&core, kept), "{kept} was not below m1");
    }

    // A plain chat's side chats go with it too.
    delete(&core, "plain").await;
    assert!(!exists(&core, "side"), "a side chat outlived its parent");
    assert!(exists(&core, "orchestrator-a"));
    core.shutdown().await;
}

#[tokio::test]
async fn deleting_a_root_takes_a_tree_deeper_than_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let core = EngineCore::assemble(
        dir.path(),
        Arc::new(HarnessRegistry::new()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    // A chain past MAX_DEPTH, as older data may hold.
    let root = "orchestrator-deep";
    create(&core, root, None);
    let mut parent = root.to_owned();
    for level in 1..=zeron_proto::orchestrator::MAX_DEPTH + 2 {
        let id = format!("level-{level}");
        create(&core, &id, Some(&parent));
        parent = id;
    }
    create(&core, "unrelated", None);

    delete(&core, root).await;
    let chats = core.workspace.read_chats().unwrap();
    let ids: Vec<&str> = chats.iter().map(|chat| chat.id.as_str()).collect();
    assert_eq!(ids, ["unrelated"], "every level went with the root");
    core.shutdown().await;
}
