//! The fork must never reach upstream's release feed. Integration tests build
//! the library without `cfg(test)`, exactly as the app does.

#[tokio::test]
async fn release_checks_and_downloads_are_refused_without_network() {
    assert!(zeron_update::self_update_disabled());
    // An unroutable edge proves the refusal happens before any request.
    let err = zeron_update::fetch_latest("http://127.0.0.1:9")
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains(zeron_update::SELF_UPDATE_DISABLED));
}

#[tokio::test]
async fn background_checker_never_reports_an_update() {
    let updater = zeron_update::Updater::spawn_desktop("http://127.0.0.1:9".into());
    updater.check_now();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let status = updater.watch().borrow().clone();
    assert!(!status.update_available);
    assert!(status.latest_version.is_none());
    assert!(updater.check().await.is_err());
}
