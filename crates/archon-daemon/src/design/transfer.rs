//! The operator answers what the preview hands over: file choosers and drops,
//! downloads, native pickers and going back. See `files.rs` for the limits.

use std::sync::Arc;

use archon_core::ArchonError;
use archon_core::design::DesignSessionId;
use archon_core::protocol::{DesignEvent, DesignPickerOption, Reply};
use archon_design_browser as adb;

use super::DesignService;
use super::browser::{PreviewBrowser, Runtime};
use super::files::Part;
use crate::context::DaemonContext;

/// Longest picker answer: a date, time, colour or option index.
const MAX_PICKER_VALUE: usize = 64;

fn unavailable(error: adb::BrowserError) -> ArchonError {
    ArchonError::Unavailable(format!("the preview browser: {error}"))
}

/// The operator-facing message for a pop-up opened in the preview instead.
pub(crate) fn new_tab_message(url: &str) -> String {
    if url.is_empty() {
        "The app tried to open an empty pop-up window. The preview shows one tab, so it stayed on this page.".into()
    } else {
        "The app opened a new tab. It is shown here; press Back to return.".into()
    }
}

/// Map a browser event about files or pickers onto the operator's stream.
pub(crate) async fn report(rt: &Runtime, event: adb::BrowserEvent) -> Option<DesignEvent> {
    Some(match event {
        adb::BrowserEvent::FileChooser {
            node,
            multiple,
            accept,
        } => {
            let id = rt.files.lock().await.open_chooser(node, multiple);
            DesignEvent::FileChooser {
                id,
                multiple,
                accept,
            }
        }
        adb::BrowserEvent::Downloaded { name, path, bytes } => {
            let id = rt
                .files
                .lock()
                .await
                .add_download(name.clone(), path, bytes);
            DesignEvent::Download {
                id,
                name,
                size: bytes,
            }
        }
        adb::BrowserEvent::DownloadFailed { name, reason } => DesignEvent::Toast {
            message: format!("The app's download of {name} was not kept: {reason}."),
        },
        adb::BrowserEvent::Picker(picker) => picker_event(picker),
        adb::BrowserEvent::Copied { text } => DesignEvent::Clipboard { text },
        adb::BrowserEvent::NewTab { url } => DesignEvent::Toast {
            message: new_tab_message(&url),
        },
        _ => return None,
    })
}

fn picker_event(picker: adb::PickerRequest) -> DesignEvent {
    let options = picker
        .options
        .into_iter()
        .map(|o| DesignPickerOption {
            label: o.label,
            value: o.value,
            selected: o.selected,
            disabled: o.disabled,
            group: o.group,
        })
        .collect();
    DesignEvent::Picker {
        id: picker.id,
        control: picker.control,
        value: picker.value,
        min: picker.min,
        max: picker.max,
        step: picker.step,
        options,
    }
}

impl DesignService {
    async fn live_runtime(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
    ) -> Result<(Arc<Runtime>, Arc<dyn PreviewBrowser>), ArchonError> {
        let row = self.current(ctx, session).await?;
        let rt = self
            .existing_runtime(&row.id)
            .ok_or_else(|| ArchonError::Unavailable("there is no preview browser".into()))?;
        let browser = rt
            .browser()
            .ok_or_else(|| ArchonError::Unavailable("there is no preview browser".into()))?;
        Ok((rt, browser))
    }

    /// One part of a file the operator chose, into the session's private directory.
    pub(crate) async fn file_part(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
        part: Part<'_>,
    ) -> Result<Reply, ArchonError> {
        let (rt, browser) = self.live_runtime(ctx, session).await?;
        // Stopping removes the profile under the gate: no part may land after it.
        let _gate = rt.gate.lock().await;
        if !rt
            .browser()
            .is_some_and(|current| Arc::ptr_eq(&current, &browser))
        {
            return Err(ArchonError::Unavailable(
                "the preview restarted; choose the files again".into(),
            ));
        }
        let root = self.profile_dir(session).join("uploads");
        rt.files.lock().await.write_part(&root, part).await?;
        Ok(Reply::DesignDone)
    }

    /// Set a transfer's files on the open chooser, or cancel it.
    pub(crate) async fn file_set(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
        chooser: u64,
        transfer: Option<&str>,
    ) -> Result<Reply, ArchonError> {
        let (rt, browser) = self.live_runtime(ctx, session).await?;
        let (open, paths) = {
            let mut transfers = rt.files.lock().await;
            let paths = transfer.map(|t| transfers.files(t)).transpose()?;
            (transfers.take_chooser(chooser)?, paths)
        };
        let Some(paths) = paths else {
            return Ok(Reply::DesignDone);
        };
        if !open.multiple && paths.len() > 1 {
            return Err(ArchonError::Invalid("the app takes one file here".into()));
        }
        browser
            .set_files(open.node, &paths)
            .await
            .map_err(unavailable)?;
        Ok(Reply::DesignDone)
    }

    /// Drop a transfer's files on the page at `(x, y)`.
    pub(crate) async fn file_drop(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
        transfer: &str,
        (x, y): (f64, f64),
    ) -> Result<Reply, ArchonError> {
        if !(x.is_finite()
            && y.is_finite()
            && (0.0..=10_000.0).contains(&x)
            && (0.0..=10_000.0).contains(&y))
        {
            return Err(ArchonError::Invalid("a drop point is on the page".into()));
        }
        let (rt, browser) = self.live_runtime(ctx, session).await?;
        let paths = rt.files.lock().await.files(transfer)?;
        browser
            .drop_files(x, y, &paths)
            .await
            .map_err(unavailable)?;
        Ok(Reply::DesignDone)
    }

    /// Part of a file the page downloaded.
    pub(crate) async fn download_part(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
        id: u64,
        offset: u64,
    ) -> Result<Reply, ArchonError> {
        use base64::Engine;
        let (rt, _) = self.live_runtime(ctx, session).await?;
        let (name, size, bytes) = rt.files.lock().await.read_download(id, offset).await?;
        assert!(
            offset + bytes.len() as u64 <= size,
            "a part stays in its download"
        );
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        Ok(Reply::DesignDownload {
            id,
            name,
            size,
            offset,
            data,
        })
    }

    /// Answer the page's native picker.
    pub(crate) async fn picker_answer(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
        id: u64,
        value: Option<&str>,
    ) -> Result<Reply, ArchonError> {
        if id == 0
            || value.is_some_and(|v| v.len() > MAX_PICKER_VALUE || v.chars().any(char::is_control))
        {
            return Err(ArchonError::Invalid("that is not a picker answer".into()));
        }
        let (_, browser) = self.live_runtime(ctx, session).await?;
        if !browser
            .answer_picker(id, value)
            .await
            .map_err(unavailable)?
            && value.is_some()
        {
            return Err(ArchonError::Invalid(
                "that choice is no longer open in the app".into(),
            ));
        }
        Ok(Reply::DesignDone)
    }

    /// Back one page in the preview's history.
    pub(crate) async fn go_back(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
    ) -> Result<Reply, ArchonError> {
        let (rt, browser) = self.live_runtime(ctx, session).await?;
        if !browser.go_back().await.map_err(unavailable)? {
            let message = "There is no earlier page in the preview.".to_owned();
            let _ = rt.events.send(DesignEvent::Toast { message });
        }
        Ok(Reply::DesignDone)
    }
}
