//! Run the delete guard's acceptance corpus against the production Pi
//! extension, and check every child Pi a guarded Pi starts gets it too.
#[tokio::test]
async fn delete_guard_blocks_deletes_and_leaves_the_rest() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new("node")
            .arg(root.join("tests/fixtures/pi-delete-guard-test.mjs"))
            .arg(root.join("src/pi/guard.mjs"))
            .env_remove("ZERON_DELETE_GUARD_FORCE")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the corpus finishes")
    .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn delete_guard_reaches_every_child_pi() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("node")
            .arg(root.join("tests/fixtures/pi-delete-guard-child-test.mjs"))
            .arg(root.join("src/pi/guard.mjs"))
            .arg(root.join("src/pi/guard-preload.mjs"))
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the child check finishes")
    .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn session_model_restrictions_hold_in_every_pi() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new("node")
            .arg(root.join("tests/fixtures/pi-model-policy-test.mjs"))
            .arg(root.join("src/pi/guard.mjs"))
            .env_remove("ZERON_MODEL_POLICY")
            .env_remove("PI_USAGE_SURFACE")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the policy checks finish")
    .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
