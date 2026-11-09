//! The `archon.toml` of Archon before 1.0, which nothing reads: archond
//! tells the operator so as it starts.

use crate::link_common;

use archon_core::notify::NotificationKind;
use link_common::Harness;

/// archond says once that a pre-1.0 `archon.toml` is ignored, and again only
/// for a file that came back after it was removed.
#[tokio::test]
async fn an_ignored_legacy_config_is_reported_once_per_appearance() {
    let harness = Harness::new().await;
    let dir = tempfile::tempdir().unwrap();
    let note = || archon_daemon::note_legacy_config(&harness.ctx, dir.path());
    note().await.unwrap();
    assert!(
        harness
            .notifications(NotificationKind::Blocker)
            .await
            .is_empty()
    );

    let legacy = dir.path().join("archon.toml");
    std::fs::write(&legacy, "[steward]\n").unwrap();
    note().await.unwrap();
    note().await.unwrap();
    let told = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(told.len(), 1, "said once: {told:?}");
    assert!(told[0].contains("archon.toml is ignored"), "{told:?}");
    assert!(
        told[0].contains("config.toml"),
        "names the file read: {told:?}"
    );

    std::fs::remove_file(&legacy).unwrap();
    note().await.unwrap();
    std::fs::write(&legacy, "[steward]\n").unwrap();
    note().await.unwrap();
    let told = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(told.len(), 2, "a file that came back is news: {told:?}");
}

#[tokio::test]
async fn startup_warns_once_for_an_old_or_missing_gh() {
    use archon_core::testing::write_script;
    let harness = Harness::new().await;
    let dir = tempfile::tempdir().unwrap();
    let gh = dir.path().join("gh");
    write_script(&gh, "#!/bin/sh\necho 'gh version 2.45.0 (Ubuntu)'\n");
    archon_daemon::warn_gh_at_start(&harness.ctx, &gh).await;
    let notices = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("2.63.0"));
    assert!(notices[0].contains("~/.local/bin"));
    write_script(&gh, "#!/bin/sh\necho 'gh version 2.63.0'\n");
    archon_daemon::warn_gh_at_start(&harness.ctx, &gh).await;
    assert_eq!(
        harness.notifications(NotificationKind::Blocker).await.len(),
        1
    );
    archon_daemon::warn_gh_at_start(&harness.ctx, &dir.path().join("missing")).await;
    let notices = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(notices.len(), 2);
    assert!(notices[1].contains("could not run"));
}
