//! What a normal browser tab offers that a headless preview must hand to the
//! operator instead: file choosers and drops, downloads, native pickers,
//! copied text, new tabs, dialogs and going back.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::bridge::{self, BridgeMessage};
use crate::browser::Shared;
use crate::sanitize::{clean, file_name};
use crate::{Browser, BrowserError, BrowserEvent};

/// Largest download offered to the operator; a bigger one is cancelled.
pub const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
/// Most downloads under way at once; another is cancelled.
const MAX_DOWNLOADS: usize = 8;
/// Finished downloads kept on disk; older ones are deleted.
pub const KEPT_DOWNLOADS: usize = 16;

/// Downloads Chromium is writing, by its GUID, and the finished ones kept.
#[derive(Debug, Default)]
pub(crate) struct Downloads {
    names: HashMap<String, String>,
    kept: VecDeque<PathBuf>,
}

/// Delete download `guid`'s file in `dir`, if it is there.
fn delete_download(dir: &Path, guid: &str) {
    assert!(valid_guid(guid), "only a checked GUID names a file");
    let path = dir.join(guid);
    assert!(
        path.parent() == Some(dir),
        "a download stays in its directory"
    );
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        // The profile, and the file with it, is still removed when the preview stops.
        Err(error) => tracing::warn!(%error, "an untracked download was not deleted"),
    }
}

/// Download events in order. A lost one could let a download escape its
/// limits, so losing any stops the preview.
pub(crate) async fn handle_downloads(
    sh: Arc<Shared>,
    mut events: tokio::sync::broadcast::Receiver<crate::cdp::CdpEvent>,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match events.recv().await {
            Ok(event) => sh.on_download(&event.method, &event.params),
            Err(RecvError::Lagged(lost)) => {
                tracing::warn!(
                    lost,
                    "the preview sent download events faster than they were read"
                );
                sh.stop_for_policy("lost download events");
                return;
            }
            Err(RecvError::Closed) => return,
        }
    }
}

fn valid_guid(guid: &str) -> bool {
    !guid.is_empty()
        && guid.len() <= 64
        && guid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Shared {
    /// Browser-level download events, which carry no page session.
    pub(crate) fn on_download(self: &Arc<Self>, method: &str, params: &Value) {
        let Some(guid) = params["guid"].as_str().filter(|g| valid_guid(g)) else {
            return;
        };
        if method == "Browser.downloadWillBegin" {
            self.download_began(guid, params);
            return;
        }
        let received = params["receivedBytes"].as_f64().unwrap_or(0.0);
        let total = params["totalBytes"].as_f64().unwrap_or(0.0);
        let too_big = received.max(total) > MAX_DOWNLOAD_BYTES as f64;
        let state = params["state"].as_str().unwrap_or_default();
        if state == "inProgress" && !too_big {
            return;
        }
        let Some(name) = self.downloads.lock().unwrap().names.remove(guid) else {
            // Untracked downloads were cancelled as they began; one that
            // finished before its cancel landed is deleted, never kept.
            if too_big {
                self.cancel_download(guid);
            }
            if state == "completed" {
                self.delete_untracked(guid);
            }
            return;
        };
        let event = match (state, too_big) {
            ("completed", false) => self.finished_download(guid, name),
            (_, true) => {
                self.cancel_download(guid);
                // It may have finished before the cancel: its file is not kept either way.
                self.delete_untracked(guid);
                BrowserEvent::DownloadFailed {
                    name,
                    reason: format!("it is larger than {} MiB", MAX_DOWNLOAD_BYTES >> 20),
                }
            }
            _ => BrowserEvent::DownloadFailed {
                name,
                reason: "it was cancelled".into(),
            },
        };
        self.emit(event);
    }

    /// Track a new download, or cancel it when as many are under way as allowed.
    fn download_began(self: &Arc<Self>, guid: &str, params: &Value) {
        let name = file_name(params["suggestedFilename"].as_str().unwrap_or_default());
        let tracked = {
            let mut downloads = self.downloads.lock().unwrap();
            let room = downloads.names.len() < MAX_DOWNLOADS;
            if room {
                downloads.names.insert(guid.to_owned(), name.clone());
            }
            assert!(
                downloads.names.len() <= MAX_DOWNLOADS,
                "downloads stay bounded"
            );
            room
        };
        if tracked {
            return;
        }
        self.cancel_download(guid);
        self.emit(BrowserEvent::DownloadFailed {
            name,
            reason: format!("{MAX_DOWNLOADS} downloads were already under way"),
        });
    }

    fn finished_download(&self, guid: &str, name: String) -> BrowserEvent {
        let Some(dir) = &self.downloads_dir else {
            unreachable!("downloads are allowed only with a private downloads directory")
        };
        let path = dir.join(guid);
        assert!(path.starts_with(dir), "a download stays in its directory");
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() && meta.len() <= MAX_DOWNLOAD_BYTES => {
                self.keep_download(path.clone());
                BrowserEvent::Downloaded {
                    name,
                    path,
                    bytes: meta.len(),
                }
            }
            Ok(_) => {
                delete_download(dir, guid);
                BrowserEvent::DownloadFailed {
                    name,
                    reason: format!("it is larger than {} MiB", MAX_DOWNLOAD_BYTES >> 20),
                }
            }
            Err(error) => BrowserEvent::DownloadFailed {
                name,
                reason: format!("it could not be read ({})", error.kind()),
            },
        }
    }

    /// Keep a finished download on disk, deleting the oldest past the cap, so
    /// the bound holds whether or not anyone hears of the download.
    fn keep_download(&self, path: PathBuf) {
        let oldest = {
            let mut downloads = self.downloads.lock().unwrap();
            downloads.kept.push_back(path);
            let oldest = (downloads.kept.len() > KEPT_DOWNLOADS)
                .then(|| downloads.kept.pop_front())
                .flatten();
            assert!(
                downloads.kept.len() <= KEPT_DOWNLOADS,
                "kept downloads stay bounded"
            );
            oldest
        };
        let Some(oldest) = oldest else {
            return;
        };
        match std::fs::remove_file(&oldest) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            // The profile, and the file with it, is still removed when the preview stops.
            Err(error) => tracing::warn!(%error, "an old download was not deleted"),
        }
    }

    fn delete_untracked(&self, guid: &str) {
        if let Some(dir) = &self.downloads_dir {
            delete_download(dir, guid);
        }
    }

    fn cancel_download(self: &Arc<Self>, guid: &str) {
        let sh = self.clone();
        let guid = guid.to_owned();
        tokio::spawn(async move {
            let cancelled = sh
                .cdp
                .call(
                    None,
                    "Browser.cancelDownload",
                    json!({"guid": guid}),
                    crate::browser::CALL_TIMEOUT,
                )
                .await;
            if let Err(error) = cancelled {
                tracing::debug!(%error, "an oversized download could not be cancelled");
            }
        });
    }

    /// A file input opened its chooser: read its `accept`, then offer it.
    pub(crate) fn on_file_chooser(self: &Arc<Self>, params: &Value) {
        let Some(node) = params["backendNodeId"].as_i64() else {
            tracing::debug!("a file chooser named no input");
            return;
        };
        let multiple = params["mode"] == "selectMultiple";
        let sh = self.clone();
        tokio::spawn(async move {
            let accept = sh.disclose(sh.input_accept(node)).await;
            match accept {
                Ok(accept) => sh.emit(BrowserEvent::FileChooser {
                    node,
                    multiple,
                    accept,
                }),
                Err(error) => tracing::debug!(%error, "a file chooser was not offered"),
            }
        });
    }

    async fn input_accept(&self, node: i64) -> Result<String, BrowserError> {
        let described = self
            .call("DOM.describeNode", json!({"backendNodeId": node}))
            .await?;
        Ok(node_attribute(&described, "accept").unwrap_or_default())
    }

    /// The `accept` of the file input under frame point `(x, y)`, if that is one.
    async fn file_input_accept_at(&self, x: f64, y: f64) -> Result<Option<String>, BrowserError> {
        let (x, y) = self.layout().await?.hit_point(x, y);
        let hit = self
            .call(
                "DOM.getNodeForLocation",
                json!({"x": x.round() as i64, "y": y.round() as i64}),
            )
            .await?;
        let Some(node) = hit["backendNodeId"].as_i64() else {
            return Ok(None);
        };
        let described = self
            .call("DOM.describeNode", json!({"backendNodeId": node}))
            .await?;
        let is_file_input = described["node"]["nodeName"] == "INPUT"
            && node_attribute(&described, "type").is_some_and(|t| t.eq_ignore_ascii_case("file"));
        Ok(is_file_input.then(|| node_attribute(&described, "accept").unwrap_or_default()))
    }

    /// The page bridge called its binding.
    pub(crate) fn on_bridge(self: &Arc<Self>, params: &Value) {
        if params["name"] != bridge::BINDING {
            return;
        }
        let Some(message) = params["payload"].as_str().and_then(bridge::parse) else {
            tracing::debug!("the page bridge sent a malformed message");
            return;
        };
        let event = match message {
            // The page navigates away at once, so no verified read could pass:
            // the URL is told only when it is one the preview may show.
            BridgeMessage::Tab(url) if url.is_empty() || self.allows_navigation(&url) => {
                self.emit(BrowserEvent::NewTab { url });
                return;
            }
            BridgeMessage::Tab(_) => return,
            BridgeMessage::Picker(picker) => BrowserEvent::Picker(picker),
            BridgeMessage::Copy(text) => BrowserEvent::Copied { text },
        };
        self.emit_disclosed(event);
    }

    /// Emit an event carrying page content once the page is verified.
    pub(crate) fn emit_disclosed(self: &Arc<Self>, event: BrowserEvent) {
        if self.containment.is_none() {
            self.emit(event);
            return;
        }
        let sh = self.clone();
        tokio::spawn(async move {
            match sh.disclose(std::future::ready(Ok(()))).await {
                Ok(()) => sh.emit(event),
                Err(error) => tracing::debug!(%error, "page content was withheld"),
            }
        });
    }
}

/// Install the bridge and allow downloads into the private directory.
pub(crate) async fn configure(
    cdp: &crate::cdp::Cdp,
    session: &str,
    downloads: Option<&Path>,
) -> Result<(), BrowserError> {
    let timeout = crate::browser::CALL_TIMEOUT;
    let behavior = match downloads {
        Some(dir) => {
            json!({"behavior": "allowAndName", "downloadPath": dir, "eventsEnabled": true})
        }
        None => json!({"behavior": "deny"}),
    };
    cdp.call(None, "Browser.setDownloadBehavior", behavior, timeout)
        .await?;
    cdp.call(
        Some(session),
        "Runtime.addBinding",
        json!({"name": bridge::BINDING}),
        timeout,
    )
    .await?;
    cdp.call(
        Some(session),
        "Page.addScriptToEvaluateOnNewDocument",
        json!({"source": bridge::SCRIPT, "runImmediately": true}),
        timeout,
    )
    .await?;
    Ok(())
}

fn path_strings(files: &[PathBuf]) -> Result<Vec<String>, BrowserError> {
    assert!(!files.is_empty(), "at least one file is given");
    files
        .iter()
        .map(|path| {
            assert!(path.is_absolute(), "files are given by absolute path");
            path.to_str()
                .map(str::to_owned)
                .ok_or_else(|| BrowserError::Protocol("a file path is not UTF-8".into()))
        })
        .collect()
}

/// Attribute `name` of a `DOM.describeNode` reply, cleaned for display.
fn node_attribute(described: &Value, name: &str) -> Option<String> {
    let attributes = described["node"]["attributes"].as_array()?;
    attributes
        .chunks(2)
        .find(|pair| pair[0] == name)
        .and_then(|pair| pair.get(1)?.as_str().map(clean))
}

impl Browser {
    /// The `accept` of the file input at `(x, y)` in CSS px; `None` when the
    /// point is not on one.
    pub async fn accept_at(&self, x: f64, y: f64) -> Result<Option<String>, BrowserError> {
        assert!(x.is_finite() && y.is_finite(), "a drop point is finite");
        self.sh.disclose(self.sh.file_input_accept_at(x, y)).await
    }

    /// Set the chosen files on the file input whose chooser opened.
    pub async fn set_files(&self, node: i64, files: &[PathBuf]) -> Result<(), BrowserError> {
        let files = path_strings(files)?;
        self.sh
            .call(
                "DOM.setFileInputFiles",
                json!({"files": files, "backendNodeId": node}),
            )
            .await
            .map(|_| ())
    }

    /// Drag files onto the page and drop them at `(x, y)`, in CSS px.
    pub async fn drop_files(&self, x: f64, y: f64, files: &[PathBuf]) -> Result<(), BrowserError> {
        assert!(x.is_finite() && y.is_finite(), "a drop point is finite");
        let files = path_strings(files)?;
        let data = json!({"items": [], "files": files, "dragOperationsMask": 1});
        for kind in ["dragEnter", "dragOver", "drop"] {
            self.sh.input_started();
            self.sh
                .call(
                    "Input.dispatchDragEvent",
                    json!({"type": kind, "x": x, "y": y, "data": data}),
                )
                .await?;
        }
        Ok(())
    }

    /// Answer the native picker `id` the page opened; `None` cancels it.
    pub async fn answer_picker(&self, id: u64, value: Option<&str>) -> Result<bool, BrowserError> {
        assert!(id > 0, "picker ids start at 1");
        let expression = bridge::answer_expression(id, value);
        let reply = self
            .sh
            .call(
                "Runtime.evaluate",
                json!({"expression": expression, "returnByValue": true}),
            )
            .await?;
        if reply.get("exceptionDetails").is_some() {
            return Err(BrowserError::Protocol(
                "the page refused the picker answer".into(),
            ));
        }
        Ok(reply["result"]["value"] == true)
    }

    /// Go back one entry in the page's history; `false` when there is none.
    pub async fn go_back(&self) -> Result<bool, BrowserError> {
        let history = self.sh.call("Page.getNavigationHistory", json!({})).await?;
        let current = history["currentIndex"].as_u64().unwrap_or(0) as usize;
        let entries = history["entries"].as_array().cloned().unwrap_or_default();
        assert!(
            entries.is_empty() || current < entries.len(),
            "the current entry is in the history"
        );
        let Some(entry) = current.checked_sub(1).and_then(|i| entries.get(i)) else {
            return Ok(false);
        };
        let url = entry["url"].as_str().unwrap_or_default();
        if !self.sh.allows_navigation(url) {
            return Err(BrowserError::Navigation {
                url: clean(url),
                error: "the previous page is outside the preview".into(),
            });
        }
        let id = entry["id"]
            .as_i64()
            .ok_or_else(|| BrowserError::Protocol("a history entry has no id".into()))?;
        self.sh
            .call("Page.navigateToHistoryEntry", json!({"entryId": id}))
            .await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::fake_browser;

    async fn cancelled(log: &crate::testutil::Log, guid: &str) -> bool {
        for _ in 0..100 {
            let found = log
                .lock()
                .unwrap()
                .iter()
                .any(|(m, p)| m == "Browser.cancelDownload" && p["guid"] == guid);
            if found {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    }

    /// Finished downloads stay bounded on disk even when nobody hears of them.
    #[tokio::test]
    async fn only_the_latest_finished_downloads_stay_on_disk() {
        let (b, _log) = fake_browser(|_, _| None).await;
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..=KEPT_DOWNLOADS)
            .map(|i| dir.path().join(format!("g{i}")))
            .collect();
        for path in &paths {
            std::fs::write(path, b"x").unwrap();
            b.sh.keep_download(path.clone());
        }
        assert!(!paths[0].exists(), "the oldest is deleted");
        assert!(paths[1..].iter().all(|p| p.exists()), "the rest are kept");
    }

    /// A download that finished before its cancel landed is not left behind.
    #[test]
    fn an_untracked_finished_download_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("g9");
        std::fs::write(&file, b"x").unwrap();
        delete_download(dir.path(), "g9");
        assert!(!file.exists());
        delete_download(dir.path(), "g9");
    }

    /// A page starting more downloads than are tracked cannot slip one past
    /// the size check: the extra one is cancelled as it begins.
    #[tokio::test]
    async fn downloads_past_the_tracking_limit_are_cancelled() {
        let (b, log) = fake_browser(|_, _| None).await;
        let mut events = b.events();
        for i in 0..=MAX_DOWNLOADS {
            let begin = json!({"guid": format!("g{i}"), "suggestedFilename": "r.csv"});
            b.sh.on_download("Browser.downloadWillBegin", &begin);
        }
        let extra = format!("g{MAX_DOWNLOADS}");
        assert!(
            cancelled(&log, &extra).await,
            "the extra download is cancelled"
        );
        let Ok(BrowserEvent::DownloadFailed { name, reason }) = events.try_recv() else {
            panic!("the operator hears of the cancelled download");
        };
        assert_eq!(name, "r.csv");
        assert!(reason.contains("already under way"), "{reason}");

        let oversized = json!({"guid": "stray", "state": "inProgress",
            "receivedBytes": (MAX_DOWNLOAD_BYTES + 1) as f64, "totalBytes": 0.0});
        b.sh.on_download("Browser.downloadProgress", &oversized);
        assert!(
            cancelled(&log, "stray").await,
            "an untracked oversized one is cancelled"
        );
    }
}
