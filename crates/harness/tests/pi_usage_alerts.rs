//! Run the subscription-usage-alert corpus against the production Pi guard:
//! the snapshot is found, trusted only while fresh, and every Pi surface on a
//! low account is told once and kept from starting new work.
#[tokio::test]
async fn usage_alerts_reach_every_pi_surface() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("node")
            .arg(root.join("tests/fixtures/pi-usage-alerts-test.mjs"))
            .arg(root.join("src/pi/guard.mjs"))
            .arg(root.join("src/pi/guard-preload.mjs"))
            .env_remove("ZERON_USAGE_ALERTS_FILE")
            .env_remove("ZERON_DATA_DIR")
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
    assert!(String::from_utf8_lossy(&output.stdout).contains("pi usage alerts: ok"));
}
