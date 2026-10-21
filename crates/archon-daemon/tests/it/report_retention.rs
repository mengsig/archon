//! Durable report publication and workspace gc, without a live backend.
use crate::common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use archon_core::config::{Config, LavishConfig, Toggle};
use archon_core::notify::FeedEntry;
use archon_core::protocol::{ReportState, Request};
use archon_daemon::{gc, reports};
use archon_lavish::Lavish;
use common::{Harness, completed};

fn page(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir.join("assets")).unwrap();
    fs::write(dir.join("assets/screenshot.png"), "image-v1").unwrap();
    let html = dir.join("index.html");
    fs::write(&html, "<img src=\"assets/screenshot.png\">v1").unwrap();
    html
}

/// The fake review service actually reads the input and its relative asset.
fn reader(dir: &Path) -> Lavish {
    let binary = dir.join("lavish-reader");
    fs::write(
        &binary,
        r#"#!/bin/sh
set -eu
cat "$1" > "$(dirname "$0")/served.html"
cat "$(dirname "$1")/assets/screenshot.png" > "$(dirname "$0")/served.png"
printf 'session:\n  url: "http://localhost/report"\n  status: opened\n'
"#,
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    Lavish::detect(&LavishConfig {
        enabled: Toggle::On,
        binary,
    })
    .unwrap()
    .unwrap()
}

fn report_pipeline(h: &Harness, workspace: &str) {
    h.add_pipeline(
        "report-only",
        &format!(
            r#"
[pipeline]
name = "report-only"
description = "report"
version = 1
workspace = "{workspace}"
requires_repo = true
[[stage]]
id = "report"
kind = "report"
prompt = "implement.md"
"#
        ),
    );
}

#[tokio::test]
async fn lavish_reads_durable_html_and_assets_after_every_workspace_collection_path() {
    for workspace in ["scratch", "worktree"] {
        // Normal retention, a manual shorter cutoff, and forced removal.
        for (aged, older_than, force) in [
            (true, None, false),
            (false, Some(0), false),
            (false, Some(0), true),
        ] {
            let service = tempfile::tempdir().unwrap();
            let lavish = reader(service.path());
            let h = Harness::with_lavish(Config::default(), lavish.clone()).await;
            report_pipeline(&h, workspace);
            let item = h
                .enqueue_in(
                    "report-only",
                    "report",
                    archon_core::model::ModelPlan(std::collections::BTreeMap::from([(
                        "report".into(),
                        common::plan().0["ship"].clone(),
                    )])),
                )
                .await
                .unwrap();
            h.dispatch().await;
            let run = h.run(&item.id).await;
            let html = page(&run.workspace.join(".archon/reports/example"));
            // Leave a clean worktree for the unforced cases; publication is
            // independent of whether the source directory is tracked.
            if workspace == "worktree" && !force {
                common::git(&run.workspace, &["add", ".archon/reports"]);
                common::git(&run.workspace, &["commit", "--quiet", "-m", "report"]);
            }
            let published = reports::publish(&h.ctx, "Example", &html, Some(&item.id))
                .await
                .unwrap();
            assert!(published.published);
            let stored = reports::list(&h.ctx).await.unwrap().remove(0).path;
            assert_eq!(
                stored,
                h.ctx
                    .paths
                    .reports
                    .join(published.report.as_str())
                    .join("index.html")
            );
            h.settle_running(|_| completed("report")).await;
            if aged {
                let then = archon_core::now() - chrono::Duration::days(8);
                let mut run = h.run(&item.id).await;
                run.finished_at = Some(then);
                let mut store = h.ctx.store.lock().await;
                store.update_run(&run).unwrap();
                let mut item = store.item(&item.id).unwrap();
                item.updated_at = then;
                store.update_item(&item).unwrap();
            }
            let pass = gc::collect(
                &h.ctx,
                older_than.map(archon_core::age::Age::from_secs),
                force,
            )
            .await
            .unwrap();
            assert!(pass.failed.is_empty(), "{pass:?}");
            assert_eq!(pass.removed, vec![run.workspace.clone()], "{pass:?}");
            assert!(!run.workspace.exists());
            reports::reopen(&h.ctx, &published.report).await.unwrap();
            assert_eq!(
                fs::read_to_string(service.path().join("served.html")).unwrap(),
                "<img src=\"assets/screenshot.png\">v1"
            );
            assert_eq!(
                fs::read_to_string(service.path().join("served.png")).unwrap(),
                "image-v1"
            );
        }
    }
}

#[tokio::test]
async fn republishing_refreshes_assets_and_pruning_removes_only_the_copy() {
    let h = Harness::new().await;
    let source = h.dir.path().join("source");
    let html = page(&source);
    let first = reports::publish(&h.ctx, "First", &html, None)
        .await
        .unwrap();
    let stored = PathBuf::from(&first.location);
    fs::write(&html, "v2").unwrap();
    fs::write(source.join("assets/screenshot.png"), "image-v2").unwrap();
    fs::write(source.join("obsolete.txt"), "obsolete").unwrap();
    reports::publish(&h.ctx, "Second", &html, None)
        .await
        .unwrap();
    fs::remove_file(source.join("obsolete.txt")).unwrap();
    let again = reports::publish(&h.ctx, "Third", &html, None)
        .await
        .unwrap();
    assert_eq!(again.report, first.report);
    assert_eq!(again.location, first.location);
    assert_eq!(fs::read_to_string(&stored).unwrap(), "v2");
    assert_eq!(
        fs::read_to_string(stored.parent().unwrap().join("assets/screenshot.png")).unwrap(),
        "image-v2"
    );
    assert!(!stored.parent().unwrap().join("obsolete.txt").exists());
    let pruned = reports::prune(
        &mut *h.ctx.store.lock().await,
        &h.ctx.paths.reports,
        archon_core::now() + chrono::Duration::days(8),
    )
    .unwrap();
    assert_eq!(pruned, 1);
    assert!(!stored.parent().unwrap().exists());
    assert!(html.exists(), "pruning never removes the source");
    assert!(reports::list(&h.ctx).await.unwrap().is_empty());
}

async fn assert_fallback(h: &Harness, title: &str, html: &Path) -> archon_core::id::ReportId {
    let shown = reports::publish(&h.ctx, title, html, None).await.unwrap();
    let record = reports::list(&h.ctx)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.report == shown.report)
        .unwrap();
    assert_eq!(record.path, html.canonicalize().unwrap());
    let feed = h.ctx.store.lock().await.feed_after(0, 10_000).unwrap();
    assert!(feed.iter().any(|f| matches!(&f.entry, FeedEntry::System { text }
        if text.contains(title) && text.contains(&html.canonicalize().unwrap().display().to_string())
        && text.contains("could not be copied") && text.contains("not durable")
        && text.contains("workspace is collected"))), "{feed:?}");
    shown.report
}

#[tokio::test]
async fn unsafe_or_oversized_refresh_falls_back_and_can_become_durable_again() {
    let service = tempfile::tempdir().unwrap();
    let h = Harness::with_lavish(Config::default(), reader(service.path())).await;
    let source = h.dir.path().join("source");
    let html = page(&source);
    let published = reports::publish(&h.ctx, "Safe", &html, None).await.unwrap();
    fs::write(&html, "revision").unwrap();
    let outside = h.dir.path().join("secret");
    fs::write(&outside, "do not copy").unwrap();
    for (target, link) in [
        (outside.as_path(), source.join("linked-file")),
        (h.dir.path(), source.join("assets/linked-dir")),
    ] {
        symlink(target, &link).unwrap();
        assert_eq!(assert_fallback(&h, "Unsafe", &html).await, published.report);
        fs::remove_file(link).unwrap();
    }
    let huge = source.join("huge");
    fs::File::create(&huge)
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    assert_eq!(
        assert_fallback(&h, "Too large", &html).await,
        published.report
    );
    assert_eq!(
        fs::read_to_string(service.path().join("served.html")).unwrap(),
        "revision"
    );
    fs::remove_file(huge).unwrap();
    let again = reports::publish(&h.ctx, "Durable again", &html, None)
        .await
        .unwrap();
    assert_eq!(again.report, published.report);
    assert_eq!(again.location, published.location);
    let recorded = reports::list(&h.ctx).await.unwrap().remove(0);
    assert!(recorded.path.starts_with(&h.ctx.paths.reports));
    assert_eq!(fs::read_to_string(recorded.path).unwrap(), "revision");
    assert_eq!(fs::read_dir(&h.ctx.paths.reports).unwrap().count(), 1);
}

#[tokio::test]
async fn symlinked_html_directories_and_parent_traversal_use_the_original() {
    let h = Harness::new().await;
    let source = h.dir.path().join("source");
    let html = page(&source);
    let link = source.join("link.html");
    symlink(&html, &link).unwrap();
    let id = assert_fallback(&h, "Linked", &link).await;
    fs::remove_file(link).unwrap();
    let alias = h.dir.path().join("alias");
    symlink(&source, &alias).unwrap();
    assert_eq!(
        assert_fallback(&h, "Alias", &alias.join("index.html")).await,
        id
    );
    let escape = source.join("assets/../../source/index.html");
    assert_eq!(assert_fallback(&h, "Traversal", &escape).await, id);
    assert_eq!(reports::list(&h.ctx).await.unwrap().len(), 1);
    assert_eq!(fs::read_dir(&h.ctx.paths.reports).unwrap().count(), 0);
    let _files = h.ctx.report_files.lock().await;
    assert_eq!(
        reports::prune(
            &mut *h.ctx.store.lock().await,
            &h.ctx.paths.reports,
            archon_core::now() + chrono::Duration::days(8)
        )
        .unwrap(),
        1
    );
    assert!(html.exists(), "fallback pruning must preserve the original");
}

#[tokio::test]
async fn legacy_reports_reopen_from_their_original_file() {
    let service = tempfile::tempdir().unwrap();
    let lavish = reader(service.path());
    let h = Harness::with_lavish(Config::default(), lavish).await;
    let html = page(&h.dir.path().join("legacy"));
    let id = archon_core::id::ReportId::new();
    let raw = serde_json::json!({"html": html, "title": "Old", "item": null, "run": null, "state": "ended"});
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&format!("report:{id}"), &raw.to_string())
        .unwrap();
    reports::reopen(&h.ctx, &id).await.unwrap();
    assert!(
        fs::read_to_string(service.path().join("served.html"))
            .unwrap()
            .ends_with("v1")
    );
    // Republishing migrates the same id to a durable copy.
    let migrated = reports::publish(&h.ctx, "Migrated", &html, None)
        .await
        .unwrap();
    assert_eq!(migrated.report, id);
    let stored = reports::list(&h.ctx).await.unwrap().remove(0).path;
    assert!(stored.starts_with(&h.ctx.paths.reports));
    h.cli(Request::ListReports).await.unwrap();
}

#[tokio::test]
async fn special_files_depth_and_entry_overflow_fall_back_without_leaving_snapshots() {
    let h = Harness::new().await;
    let source = h.dir.path().join("bounded");
    let html = page(&source);
    let fifo = source.join("pipe");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR,
        0,
    )
    .unwrap();
    assert_fallback(&h, "FIFO", &html).await;
    fs::remove_file(fifo).unwrap();
    let deep = source.join("deep");
    let mut path = deep.clone();
    for _ in 0..33 {
        fs::create_dir(&path).unwrap();
        path = path.join("next");
    }
    assert_fallback(&h, "Deep", &html).await;
    fs::remove_dir_all(deep).unwrap();
    let entries = source.join("entries");
    fs::create_dir(&entries).unwrap();
    for n in 0..4096 {
        fs::write(entries.join(n.to_string()), []).unwrap();
    }
    assert_fallback(&h, "Many", &html).await;
    fs::remove_dir_all(entries).unwrap();
    assert_eq!(fs::read_dir(&h.ctx.paths.reports).unwrap().count(), 0);
    assert_eq!(reports::list(&h.ctx).await.unwrap().len(), 1);
    assert!(
        reports::publish(&h.ctx, "Within limits", &html, None)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn pruning_cannot_follow_an_archive_symlink_or_delete_its_record_on_failure() {
    let h = Harness::new().await;
    let html = page(&h.dir.path().join("source"));
    let published = reports::publish(&h.ctx, "Report", &html, None)
        .await
        .unwrap();
    let archive = h.ctx.paths.reports.join(published.report.as_str());
    fs::remove_dir_all(&archive).unwrap();
    let outside = h.dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), "safe").unwrap();
    symlink(&outside, &archive).unwrap();
    assert!(
        reports::prune(
            &mut *h.ctx.store.lock().await,
            &h.ctx.paths.reports,
            archon_core::now() + chrono::Duration::days(8)
        )
        .is_err()
    );
    assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "safe");
    assert_eq!(
        reports::list(&h.ctx).await.unwrap().len(),
        1,
        "failure remains retryable"
    );
}

/// A handshake holds publication after the snapshot is installed, without
/// a network service or an assumed delay in publication.
fn held_reader(dir: &Path) -> Lavish {
    let lavish = reader(dir);
    let script = fs::read_to_string(lavish.binary()).unwrap();
    fs::write(
        lavish.binary(),
        script.replace(
            "set -eu",
            r#"set -eu
: > "$(dirname "$0")/entered"
while [ ! -f "$(dirname "$0")/release" ]; do sleep 0.01; done"#,
        ),
    )
    .unwrap();
    lavish
}

async fn entered(dir: &Path) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !dir.join("entered").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fake Lavish was entered");
}

#[tokio::test]
async fn cancelled_first_publication_and_refresh_do_not_leak_or_replace_the_previous_copy() {
    for refresh in [false, true] {
        let service = tempfile::tempdir().unwrap();
        let h = Harness::with_lavish(Config::default(), held_reader(service.path())).await;
        let html = page(&h.dir.path().join("source"));
        fs::write(service.path().join("release"), "go").unwrap();
        let previous = if refresh {
            Some(
                reports::publish(&h.ctx, "First", &html, None)
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        fs::remove_file(service.path().join("release")).unwrap();
        if refresh {
            fs::remove_file(service.path().join("entered")).unwrap();
        }
        fs::write(&html, "new revision").unwrap();
        let ctx = h.ctx.clone();
        let task = tokio::spawn(async move { reports::publish(&ctx, "New", &html, None).await });
        entered(service.path()).await;
        assert!(
            h.ctx.report_files.try_lock().is_err(),
            "publication holds the files lock"
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let _files = h.ctx.report_files.lock().await;
        reports::prune(
            &mut *h.ctx.store.lock().await,
            &h.ctx.paths.reports,
            archon_core::now() + chrono::Duration::days(365),
        )
        .unwrap();
        if let Some(previous) = previous {
            let record = reports::list(&h.ctx).await.unwrap().remove(0);
            assert_eq!(record.report, previous.report);
            assert!(fs::read_to_string(record.path).unwrap().ends_with("v1"));
            assert_eq!(fs::read_dir(&h.ctx.paths.reports).unwrap().count(), 1);
        } else {
            assert!(reports::list(&h.ctx).await.unwrap().is_empty());
            assert_eq!(fs::read_dir(&h.ctx.paths.reports).unwrap().count(), 0);
        }
    }
}

#[tokio::test]
async fn publication_store_error_removes_a_first_copy_and_restores_a_refresh() {
    for refresh in [false, true] {
        let service = tempfile::tempdir().unwrap();
        let h = Harness::with_lavish(Config::default(), held_reader(service.path())).await;
        let html = page(&h.dir.path().join("source"));
        fs::write(service.path().join("release"), "go").unwrap();
        let previous = if refresh {
            Some(
                reports::publish(&h.ctx, "First", &html, None)
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        fs::remove_file(service.path().join("release")).unwrap();
        if refresh {
            fs::remove_file(service.path().join("entered")).unwrap();
        }
        fs::write(&html, "new revision").unwrap();
        let ctx = h.ctx.clone();
        let task = tokio::spawn(async move { reports::publish(&ctx, "New", &html, None).await });
        entered(service.path()).await;
        // The next record lookup fails in announce, after copying and opening.
        h.ctx
            .store
            .lock()
            .await
            .kv_set(reports::LEGACY_KEY, "{")
            .unwrap();
        fs::write(service.path().join("release"), "go").unwrap();
        let error = task.await.unwrap().unwrap_err();
        assert!(
            matches!(error, archon_core::error::ArchonError::Corrupt(_)),
            "{error}"
        );
        h.ctx
            .store
            .lock()
            .await
            .kv_delete(reports::LEGACY_KEY)
            .unwrap();
        let records = reports::list(&h.ctx).await.unwrap();
        if let Some(previous) = previous {
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].report, previous.report);
            assert_eq!(records[0].title, "First");
            assert!(
                fs::read_to_string(&records[0].path)
                    .unwrap()
                    .ends_with("v1")
            );
            assert_eq!(fs::read_dir(&h.ctx.paths.reports).unwrap().count(), 1);
        } else {
            assert!(records.is_empty());
            assert_eq!(fs::read_dir(&h.ctx.paths.reports).unwrap().count(), 0);
        }
    }
}

#[tokio::test]
async fn orphan_pruning_obeys_names_mtime_and_symlinks_and_keeps_recorded_copies() {
    use archon_core::id::ReportId;
    let h = Harness::new().await;
    let html = page(&h.dir.path().join("source"));
    let shown = reports::publish(&h.ctx, "Kept", &html, None).await.unwrap();
    let cutoff = archon_core::now() - chrono::Duration::days(7);
    let old: std::time::SystemTime = (cutoff - chrono::Duration::days(1)).into();
    let age = |path: &Path| {
        fs::File::open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
    };
    let archive = h.ctx.paths.reports.join(shown.report.as_str());
    age(&archive);
    let orphan = h.ctx.paths.reports.join(ReportId::new().as_str());
    let staged = h
        .ctx
        .paths
        .reports
        .join(format!(".{}-{}", shown.report, ReportId::new()));
    let protected_staging =
        h.ctx
            .paths
            .reports
            .join(format!(".{}-{}", ReportId::new(), ReportId::new()));
    fs::create_dir(&protected_staging).unwrap();
    let protected_html = protected_staging.join("index.html");
    fs::write(&protected_html, "legacy").unwrap();
    let legacy_id = ReportId::new();
    let legacy = serde_json::json!({"html": protected_html, "title": "legacy", "item": null, "run": null, "state": "unpublished"});
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&format!("report:{legacy_id}"), &legacy.to_string())
        .unwrap();
    age(&protected_staging);
    let recent_staging =
        h.ctx
            .paths
            .reports
            .join(format!(".{}-{}", ReportId::new(), ReportId::new()));
    fs::create_dir(&recent_staging).unwrap();
    let boundary = h.ctx.paths.reports.join(ReportId::new().as_str());
    fs::create_dir(&boundary).unwrap();
    fs::File::open(&boundary)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(cutoff.into()))
        .unwrap();
    let recent = h.ctx.paths.reports.join(ReportId::new().as_str());
    let foreign = h.ctx.paths.reports.join("rpt_other");
    let fake_staging = h
        .ctx
        .paths
        .reports
        .join(format!(".{}-other", ReportId::new()));
    for path in [&orphan, &staged, &recent, &foreign, &fake_staging] {
        fs::create_dir(path).unwrap();
        if path != &recent {
            age(path);
        }
    }
    let outside = h.dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), "safe").unwrap();
    age(&outside);
    let link = h.ctx.paths.reports.join(ReportId::new().as_str());
    symlink(&outside, &link).unwrap();
    let file = h.ctx.paths.reports.join(ReportId::new().as_str());
    fs::write(&file, "keep").unwrap();
    age(&file);
    let _files = h.ctx.report_files.lock().await;
    reports::prune(&mut *h.ctx.store.lock().await, &h.ctx.paths.reports, cutoff).unwrap();
    assert!(!orphan.exists());
    assert!(!staged.exists());
    for path in [
        &archive,
        &recent,
        &foreign,
        &fake_staging,
        &link,
        &file,
        &protected_staging,
        &recent_staging,
        &boundary,
    ] {
        assert!(path.exists(), "{}", path.display());
    }
    assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "safe");
}

#[tokio::test]
async fn archive_containment_uses_a_visible_non_durable_fallback() {
    let h = Harness::new().await;
    let html = h.ctx.paths.data_dir.join("index.html");
    fs::write(&html, "report").unwrap();
    assert_fallback(&h, "Contains archive", &html).await;
    let _files = h.ctx.report_files.lock().await;
    reports::prune(
        &mut *h.ctx.store.lock().await,
        &h.ctx.paths.reports,
        archon_core::now() + chrono::Duration::days(365),
    )
    .unwrap();
    assert!(html.exists());
}

#[tokio::test]
async fn stages_publish_copy_refusals_and_visibly_refuse_workspace_escapes() {
    for escape in [false, true] {
        let service = tempfile::tempdir().unwrap();
        let h = Harness::with_lavish(Config::default(), reader(service.path())).await;
        report_pipeline(&h, "scratch");
        let item = h
            .enqueue_in(
                "report-only",
                "stage report",
                archon_core::model::ModelPlan(std::collections::BTreeMap::from([(
                    "report".into(),
                    common::plan().0["ship"].clone(),
                )])),
            )
            .await
            .unwrap();
        h.dispatch().await;
        let run = h.run(&item.id).await;
        let html = page(&run.workspace.join(".archon/reports/example"));
        let listed = if escape {
            let outside = page(&h.dir.path().join("outside"));
            let link = run.workspace.join("outside.html");
            symlink(outside, &link).unwrap();
            link
        } else {
            symlink(&html, html.parent().unwrap().join("linked-asset")).unwrap();
            html.clone()
        };
        h.settle_running(|_| archon_core::run::StageOutcome::Completed {
            summary: "report".into(),
            structured: Some(serde_json::json!({"reports": [listed]})),
        })
        .await;
        let records = reports::list(&h.ctx).await.unwrap();
        let feed = h.ctx.store.lock().await.feed_after(0, 10_000).unwrap();
        if escape {
            assert!(records.is_empty());
            assert!(
                feed.iter()
                    .any(|f| matches!(&f.entry, FeedEntry::System { text }
                if text.contains("did not publish") && text.contains("outside"))),
                "{feed:?}"
            );
        } else {
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].path, html);
            assert_eq!(records[0].state, ReportState::Open);
            assert!(
                feed.iter()
                    .any(|f| matches!(&f.entry, FeedEntry::System { text }
                if text.contains("not durable") && text.contains("workspace is collected"))),
                "{feed:?}"
            );
            assert!(
                fs::read_to_string(service.path().join("served.html"))
                    .unwrap()
                    .ends_with("v1")
            );
        }
    }
}

#[tokio::test]
async fn a_failed_legacy_refresh_keeps_the_record_and_leaves_its_new_copy_for_orphan_pruning() {
    let service = tempfile::tempdir().unwrap();
    let h = Harness::with_lavish(Config::default(), held_reader(service.path())).await;
    let html = page(&h.dir.path().join("legacy"));
    let report = archon_core::id::ReportId::new();
    let raw = serde_json::json!({"html": html, "title": "Legacy", "item": null, "run": null, "state": "unpublished"});
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&format!("report:{report}"), &raw.to_string())
        .unwrap();
    let ctx = h.ctx.clone();
    let input = html.clone();
    let task = tokio::spawn(async move { reports::publish(&ctx, "New", &input, None).await });
    entered(service.path()).await;
    h.ctx
        .store
        .lock()
        .await
        .kv_set(reports::LEGACY_KEY, "{")
        .unwrap();
    fs::write(service.path().join("release"), "go").unwrap();
    assert!(matches!(
        task.await.unwrap().unwrap_err(),
        archon_core::error::ArchonError::Corrupt(_)
    ));
    h.ctx
        .store
        .lock()
        .await
        .kv_delete(reports::LEGACY_KEY)
        .unwrap();
    let record = reports::list(&h.ctx).await.unwrap().remove(0);
    assert_eq!(record.path, html);
    assert_eq!(record.title, "Legacy");
    let archive = h.ctx.paths.reports.join(report.as_str());
    assert!(
        archive.exists(),
        "failure must not remove an already recorded id's directory"
    );
    let _files = h.ctx.report_files.lock().await;
    reports::prune(
        &mut *h.ctx.store.lock().await,
        &h.ctx.paths.reports,
        archon_core::now() + chrono::Duration::days(365),
    )
    .unwrap();
    assert!(!archive.exists());
    assert!(html.exists());
}

/// Age the durable record independently of item settlement, as a quiet review
/// surface can be much older than the work it still protects.
async fn age_record(h: &Harness, id: &archon_core::id::ReportId, state: &str) {
    let key = format!("report:{id}");
    let mut store = h.ctx.store.lock().await;
    let mut record: serde_json::Value =
        serde_json::from_str(&store.kv_get(&key).unwrap().unwrap()).unwrap();
    record["updated_at"] = serde_json::json!(archon_core::now() - chrono::Duration::days(16));
    record["state"] = serde_json::json!(state);
    store.kv_set(&key, &record.to_string()).unwrap();
}

#[tokio::test]
async fn expiry_requires_old_settled_items_and_quiet_reports_and_keeps_sources_and_history() {
    for case in [
        "expired",
        "unpublished",
        "live",
        "recent_item",
        "recent_report",
        "open",
        "question",
        "disabled",
        "symlink",
    ] {
        let mut config = Config::default();
        if case == "disabled" {
            config.storage.retention_days = 0;
        }
        let h = Harness::with_config(config).await;
        let item = h.enqueue(case).await;
        let source = page(&h.dir.path().join("source"));
        let published = reports::publish(&h.ctx, "Review", &source, Some(&item.id))
            .await
            .unwrap();
        let stored = reports::list(&h.ctx).await.unwrap().remove(0).path;
        if case != "live" {
            h.cli(Request::CancelItem {
                item: item.id.clone(),
                reason: None,
            })
            .await
            .unwrap();
            if case != "recent_item" {
                let mut store = h.ctx.store.lock().await;
                let mut item = store.item(&item.id).unwrap();
                item.updated_at = archon_core::now() - chrono::Duration::days(16);
                store.update_item(&item).unwrap();
            }
        }
        if case != "recent_report" {
            age_record(
                &h,
                &published.report,
                if case == "open" {
                    "open"
                } else if matches!(case, "unpublished" | "live" | "disabled") {
                    "unpublished"
                } else {
                    "ended"
                },
            )
            .await;
        }
        if case == "question" {
            h.ctx
                .store
                .lock()
                .await
                .insert_question(&archon_core::notify::Question {
                    id: archon_core::id::QuestionId::new(),
                    number: 0,
                    text: "Still needed?".into(),
                    options: vec![],
                    item: Some(item.id.clone()),
                    aether: None,
                    asked_by: None,
                    asked_at: archon_core::now(),
                    answer: None,
                    answered_at: None,
                })
                .unwrap();
        }
        let outside = h.dir.path().join("outside");
        if case == "symlink" {
            fs::rename(stored.parent().unwrap(), &outside).unwrap();
            symlink(&outside, stored.parent().unwrap()).unwrap();
        }
        reports::prune(
            &mut *h.ctx.store.lock().await,
            &h.ctx.paths.reports,
            archon_core::now() - chrono::Duration::days(7),
        )
        .unwrap();
        assert!(
            stored.exists(),
            "the metadata prune leaves owned snapshots to filesystem GC"
        );
        let pass = gc::collect(&h.ctx, None, false).await.unwrap();
        if matches!(case, "expired" | "unpublished") {
            assert_eq!(pass.reports.len(), 1, "{case}: {pass:?}");
            assert!(pass.bytes_freed > 0);
            assert!(!stored.exists());
            let again = gc::collect(&h.ctx, None, false).await.unwrap();
            assert!(
                again.reports.is_empty(),
                "already expired files do not count twice"
            );
            let archon_core::protocol::Reply::Status(status) =
                h.cli(Request::Status(Default::default())).await.unwrap()
            else {
                panic!("status")
            };
            let disk = status.disk.unwrap();
            assert_eq!(disk.gc.unwrap().bytes_freed, pass.bytes_freed);
            assert!(
                disk.storage
                    .iter()
                    .any(|usage| usage.kind == "reports_lavish" && usage.bytes.is_some())
            );
            assert!(
                disk.storage.iter().any(|usage| usage.kind == "experiments"),
                "reviewers' experiment copies are accounted"
            );
        } else {
            assert!(stored.exists(), "{case}: {pass:?}");
            assert!(pass.reports.is_empty(), "{case}: {pass:?}");
        }
        if case == "symlink" {
            assert_eq!(pass.failed.len(), 1);
        } else {
            assert!(pass.failed.is_empty(), "{case}: {pass:?}");
        }
        assert!(source.exists(), "original source is never expired");
        assert_eq!(
            reports::list(&h.ctx).await.unwrap().len(),
            1,
            "expiry does not delete store rows"
        );
    }
}

#[tokio::test]
async fn expiry_keeps_a_settled_items_report_referenced_by_a_live_gate() {
    use archon_core::run::{GateSubject, OperatorGate};
    let h = Harness::new().await;
    let item = h.enqueue("settled report").await;
    let source = page(&h.dir.path().join("gate-source"));
    let published = reports::publish(&h.ctx, "Gate", &source, Some(&item.id))
        .await
        .unwrap();
    let stored = reports::list(&h.ctx).await.unwrap().remove(0).path;
    h.cli(Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    })
    .await
    .unwrap();
    age_record(&h, &published.report, "ended").await;
    let gate_item = h.enqueue("live gate").await;
    h.dispatch().await;
    let run = h.run(&gate_item.id).await;
    {
        let mut store = h.ctx.store.lock().await;
        let mut item = store.item(&item.id).unwrap();
        item.updated_at = archon_core::now() - chrono::Duration::days(16);
        store.update_item(&item).unwrap();
        store
            .open_gate(&OperatorGate {
                run: run.id,
                item: gate_item.id,
                stage_id: "implement".into(),
                attempt: 1,
                subject: GateSubject::Report,
                summary: "review".into(),
                page: None,
                location: None,
                report: Some(published.report),
                question: None,
                decision: None,
                withdrawn: None,
                opened_at: archon_core::now(),
                decided_at: None,
            })
            .unwrap();
    }
    let pass = gc::collect(&h.ctx, None, false).await.unwrap();
    assert!(pass.reports.is_empty(), "{pass:?}");
    assert!(stored.exists());
}

#[tokio::test]
async fn publish_report_refuses_a_page_outside_every_repository_and_workspace() {
    let h = Harness::new().await;
    let outside = page(&h.dir.path().join("elsewhere"));
    let request = |path: &Path| Request::PublishReport {
        title: "Page".into(),
        path: path.to_path_buf(),
        item: None,
    };
    let refused = h.cli(request(&outside)).await.unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("outside every registered repository"),
        "{refused}"
    );
    let in_worktrees = page(&h.ctx.paths.worktrees.join("item"));
    h.cli(request(&in_worktrees))
        .await
        .expect("a workspace page is published");
    let in_repo = page(&h.repo.join("docs"));
    h.cli(request(&in_repo))
        .await
        .expect("a repository page is published");
}
