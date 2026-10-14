//! Run the delete guard's acceptance corpus against the production Pi
//! extension.
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
