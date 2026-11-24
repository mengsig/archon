//! Browser-side document checks and the disclosure epoch.

use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use tokio::sync::broadcast;

use crate::cdp::CdpEvent;
use crate::launch::kill_if_matches;
use crate::proxy::Policy;
use crate::relay::RelayShared;
use crate::{BrowserError, BrowserEvent};

#[derive(Debug, Default)]
pub(crate) struct DocumentState {
    epoch: u64,
    main_frame: Option<String>,
    reason: Option<&'static str>,
}

#[derive(Debug)]
pub(crate) struct Containment {
    pub(crate) target: String,
    pub(crate) session: String,
    pub(crate) policy: Arc<Policy>,
    pub(crate) process: (u32, u64),
    pub(crate) events: broadcast::Sender<BrowserEvent>,
    pub(crate) relay: Arc<RelayShared>,
    pub(crate) state: Mutex<DocumentState>,
}

impl Containment {
    pub(crate) fn reason(&self) -> Option<&'static str> {
        self.state.lock().unwrap().reason
    }

    pub(crate) fn stop(&self, reason: &'static str) -> BrowserError {
        let mut state = self.state.lock().unwrap();
        if let Some(first) = state.reason {
            return BrowserError::Containment(first.into());
        }
        state.reason = Some(reason);
        self.relay.close();
        drop(state);

        tracing::warn!(
            reason,
            "stopping preview browser after document policy failure"
        );
        let (pid, started_at) = self.process;
        if let Err(error) = kill_if_matches(pid, started_at) {
            tracing::error!(%error, pid, "failed to stop preview browser after document policy failure");
        }
        let _ = self.events.send(BrowserEvent::ContainmentStopped {
            reason: reason.into(),
        });
        BrowserError::Containment(reason.into())
    }

    pub(crate) fn epoch(&self) -> Result<u64, BrowserError> {
        let state = self.state.lock().unwrap();
        match state.reason {
            Some(reason) => Err(BrowserError::Containment(reason.into())),
            None => Ok(state.epoch),
        }
    }

    pub(crate) fn confirm(
        &self,
        epoch: u64,
    ) -> Result<MutexGuard<'_, DocumentState>, BrowserError> {
        let state = self.state.lock().unwrap();
        if let Some(reason) = state.reason {
            return Err(BrowserError::Containment(reason.into()));
        }
        if state.epoch != epoch {
            return Err(BrowserError::DisclosureUnavailable);
        }
        Ok(state)
    }

    /// Runs on the CDP reader before it resolves subsequent command replies.
    pub(crate) fn observe(&self, event: &CdpEvent) -> Result<(), BrowserError> {
        if matches!(
            event.method.as_str(),
            "Target.targetCreated" | "Target.targetInfoChanged"
        ) {
            return self.check_target(&event.params["targetInfo"]);
        }
        if event.session_id.as_deref() != Some(&self.session) {
            return Ok(());
        }
        match event.method.as_str() {
            "Page.windowOpen" => {
                return Err(self.stop("the preview attempted to open a new window"));
            }
            "Page.interstitialShown" => {
                return Err(self.stop("Chromium opened a preview interstitial"));
            }
            "Page.frameNavigated" | "Page.documentOpened" => {
                let frame = &event.params["frame"];
                self.check_frame(frame, frame["parentId"].is_null())?;
            }
            "Page.navigatedWithinDocument" => {
                if !event.params["url"]
                    .as_str()
                    .is_some_and(|url| self.policy.origin.allows_navigation(url))
                {
                    return Err(self.stop("unexpected URL in a same-document navigation"));
                }
            }
            "Page.frameRequestedNavigation" | "Page.frameStartedNavigating" => {
                self.check_navigation_start(&event.params)?;
            }
            "Page.frameAttached"
            | "Page.frameDetached"
            | "Page.frameSubtreeWillBeDetached"
            | "Page.frameStartedLoading"
            | "Page.frameStoppedLoading" => {}
            _ => return Ok(()),
        }
        self.state.lock().unwrap().epoch += 1;
        Ok(())
    }

    fn check_navigation_start(&self, params: &Value) -> Result<(), BrowserError> {
        let Some(url) = nonempty(&params["url"]) else {
            return Err(self.stop("Chromium did not report an impending navigation URL"));
        };
        let Some(id) = nonempty(&params["frameId"]) else {
            return Err(self.stop("Chromium did not identify a navigating frame"));
        };
        if is_network_document(url) {
            return Ok(());
        }
        let main = {
            let state = self.state.lock().unwrap();
            if state.main_frame.is_none() && id == self.target && url == "about:blank" {
                return Ok(());
            }
            state.main_frame.as_deref() == Some(id)
        };
        let reason = if main {
            "a main-frame document tried to navigate to an unsupported URL scheme"
        } else {
            "a subframe tried to navigate to an unsupported URL scheme"
        };
        Err(self.stop(reason))
    }

    fn check_target(&self, info: &Value) -> Result<(), BrowserError> {
        match info["type"].as_str() {
            Some("page" | "webview" | "background_page") if info["targetId"] != self.target => {
                Err(self.stop("Chromium discovered an unexpected new window or page target"))
            }
            Some("iframe") => Err(self.stop("Chromium created an isolated subframe")),
            Some("page" | "browser" | "tab" | "worker" | "shared_worker" | "service_worker") => {
                Ok(())
            }
            Some("browser_ui")
                if info["url"]
                    .as_str()
                    .is_some_and(|url| url.starts_with("chrome://")) =>
            {
                Ok(())
            }
            _ => Err(self.stop("Chromium discovered an unrecognized preview target")),
        }
    }

    pub(crate) fn verify_targets(&self, result: &Value) -> Result<(), BrowserError> {
        let targets = result["targetInfos"]
            .as_array()
            .ok_or_else(|| self.stop("Chromium did not provide the preview target list"))?;
        for target in targets {
            self.check_target(target)?;
        }
        if !targets
            .iter()
            .any(|target| target["type"] == "page" && target["targetId"] == self.target)
        {
            return Err(self.stop("the preview page is missing from Chromium's target list"));
        }
        Ok(())
    }

    /// False for a startup blank or Chromium's own failed-navigation page.
    fn check_frame(&self, frame: &Value, main: bool) -> Result<bool, BrowserError> {
        let Some(url) = nonempty(&frame["url"]) else {
            return Err(self.stop("Chromium did not provide a preview document URL"));
        };
        if nonempty(&frame["id"]).is_none() || nonempty(&frame["loaderId"]).is_none() {
            return Err(self.stop("Chromium did not identify a preview document"));
        }
        if url == "chrome-error://chromewebdata/"
            && nonempty(&frame["unreachableUrl"]).is_some_and(is_network_document)
        {
            return Ok(false);
        }
        if main && url == "about:blank" && self.state.lock().unwrap().main_frame.is_none() {
            return Ok(false);
        }
        let origin = &self.policy.origin;
        if !origin.allows_navigation(url)
            || !nonempty(&frame["securityOrigin"])
                .is_some_and(|value| origin.allows_navigation(value))
        {
            let reason = if main {
                "a main-frame document was outside the preview origin"
            } else {
                "a subframe document was outside the preview origin"
            };
            return Err(self.stop(reason));
        }
        if main {
            self.state.lock().unwrap().main_frame = frame["id"].as_str().map(str::to_owned);
        }
        Ok(true)
    }

    pub(crate) fn verify_tree(&self, tree: &Value) -> Result<(), BrowserError> {
        let mut pending = vec![(tree, None)];
        let mut ready = true;
        while let Some((branch, parent)) = pending.pop() {
            let frame = &branch["frame"];
            if !parent.map_or_else(
                || frame["parentId"].is_null(),
                |id| frame["parentId"].as_str() == Some(id),
            ) {
                return Err(self.stop("Chromium reported an inconsistent preview frame tree"));
            }
            ready &= self.check_frame(frame, parent.is_none())?;
            match branch.get("childFrames") {
                Some(Value::Array(children)) => {
                    let parent = frame["id"].as_str();
                    pending.extend(children.iter().map(|child| (child, parent)));
                }
                None => {}
                _ => return Err(self.stop("Chromium reported an invalid preview frame tree")),
            }
        }
        if ready {
            Ok(())
        } else {
            Err(BrowserError::DisclosureUnavailable)
        }
    }
}

fn is_network_document(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}
