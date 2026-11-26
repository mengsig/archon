//! Browser-side document checks and the disclosure epoch.

use std::sync::{Arc, Mutex, MutexGuard};

use http::uri::Authority;
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

enum FrameStatus<'a> {
    Verified,
    Unavailable,
    NeedsStorageOrigin(&'a str),
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
            id == self.target || state.main_frame.as_deref() == Some(id)
        };
        // Disclosure still requires Chromium to verify the committed origin.
        if !main && can_inherit_origin(url) {
            return Ok(());
        }
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

    fn check_frame<'a>(
        &self,
        frame: &'a Value,
        main: bool,
    ) -> Result<FrameStatus<'a>, BrowserError> {
        let Some(url) = nonempty(&frame["url"]) else {
            return Err(self.stop("Chromium did not provide a preview document URL"));
        };
        let Some(id) = nonempty(&frame["id"]) else {
            return Err(self.stop("Chromium did not identify a preview document"));
        };
        if nonempty(&frame["loaderId"]).is_none() {
            return Err(self.stop("Chromium did not identify a preview document"));
        }
        if url == "chrome-error://chromewebdata/"
            && nonempty(&frame["unreachableUrl"]).is_some_and(is_network_document)
        {
            return Ok(FrameStatus::Unavailable);
        }
        if main && url == "about:blank" && self.state.lock().unwrap().main_frame.is_none() {
            return Ok(FrameStatus::Unavailable);
        }
        let origin = &self.policy.origin;
        let inherited = !main && can_inherit_origin(url);
        // Some Chromium versions report :// for inherited URLs in Page.Frame.
        if inherited && frame["securityOrigin"] == "://" {
            return Ok(FrameStatus::NeedsStorageOrigin(id));
        }
        let allowed_url = origin.allows_navigation(url) || inherited;
        if !allowed_url
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
            self.state.lock().unwrap().main_frame = Some(id.to_owned());
        }
        Ok(FrameStatus::Verified)
    }

    /// Checks the tree and returns the frame IDs that still need storage-origin verification.
    pub(crate) fn check_frame_tree<'a>(
        &self,
        tree: &'a Value,
    ) -> Result<Vec<&'a str>, BrowserError> {
        let mut pending = vec![(tree, None)];
        let mut ready = true;
        let mut needs_storage_origin = Vec::new();
        while let Some((branch, parent)) = pending.pop() {
            let frame = &branch["frame"];
            if !parent.map_or_else(
                || frame["parentId"].is_null(),
                |id| frame["parentId"].as_str() == Some(id),
            ) {
                return Err(self.stop("Chromium reported an inconsistent preview frame tree"));
            }
            match self.check_frame(frame, parent.is_none())? {
                FrameStatus::Verified => {}
                FrameStatus::Unavailable => ready = false,
                FrameStatus::NeedsStorageOrigin(id) => needs_storage_origin.push(id),
            }
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
            Ok(needs_storage_origin)
        } else {
            Err(BrowserError::DisclosureUnavailable)
        }
    }

    pub(crate) fn verify_storage_origin(
        &self,
        reply: Result<Value, BrowserError>,
    ) -> Result<(), BrowserError> {
        let result = match reply {
            Ok(result) => result,
            Err(error @ BrowserError::Protocol(_)) => {
                tracing::debug!(%error, "Chromium did not verify a preview subframe storage origin");
                return Err(self.stop("Chromium could not verify a subframe storage origin"));
            }
            Err(error) => return Err(error),
        };
        let authority = nonempty(&result["storageKey"])
            .and_then(|key| key.strip_prefix("http://"))
            .and_then(|key| key.strip_suffix('/'))
            .and_then(|authority| Authority::try_from(authority).ok());
        if authority.is_some_and(|authority| self.policy.origin.matches_authority(&authority)) {
            Ok(())
        } else {
            Err(self.stop("a subframe storage origin was outside the preview origin"))
        }
    }
}

fn is_network_document(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn can_inherit_origin(url: &str) -> bool {
    matches!(url, "about:blank" | "about:srcdoc")
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ORIGIN: &str = "http://127.0.0.1:3000";

    fn preview_guard() -> Containment {
        let policy = Policy::new(ORIGIN, Arc::new(crate::testutil::AcceptUpstream)).unwrap();
        let (events, _) = broadcast::channel(8);
        Containment {
            target: "main".into(),
            session: "session".into(),
            policy: Arc::new(policy),
            process: (0, 0),
            events,
            relay: Default::default(),
            state: Mutex::default(),
        }
    }

    fn child_tree(url: &str, origin: &str) -> Value {
        json!({
            "frame": {"id":"main", "loaderId":"main-loader", "url":ORIGIN, "securityOrigin":ORIGIN},
            "childFrames": [{"frame": {
                "id":"child", "parentId":"main", "loaderId":"child-loader",
                "url":url, "securityOrigin":origin
            }}]
        })
    }

    #[test]
    fn inherited_children_require_verified_frame_or_storage_origins() {
        for url in ["about:blank", "about:srcdoc"] {
            let guard = preview_guard();
            assert!(
                guard
                    .check_frame_tree(&child_tree(url, ORIGIN))
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                guard.check_frame_tree(&child_tree(url, "://")).unwrap(),
                ["child"]
            );
            guard
                .verify_storage_origin(Ok(json!({"storageKey":"http://127.0.0.1:3000/"})))
                .unwrap();
            assert!(guard.reason().is_none());

            for origin in ["http://127.0.0.1:4000", "null"] {
                let blocked = preview_guard();
                let result = blocked.check_frame_tree(&child_tree(url, origin)).map(drop);
                assert!(matches!(result, Err(BrowserError::Containment(_))));
                assert_eq!(
                    blocked.reason(),
                    Some("a subframe document was outside the preview origin")
                );
            }
        }
    }

    #[test]
    fn an_inherited_url_cannot_replace_the_main_document() {
        let guard = preview_guard();
        let tree = json!({"frame": {"id":"main", "loaderId":"loader", "url":"about:srcdoc", "securityOrigin":ORIGIN}});
        assert!(matches!(
            guard.check_frame_tree(&tree),
            Err(BrowserError::Containment(_))
        ));
        assert_eq!(
            guard.reason(),
            Some("a main-frame document was outside the preview origin")
        );
    }

    #[test]
    fn invalid_subframe_storage_keys_stop_disclosure() {
        for result in [
            json!({"storageKey":"http://127.0.0.1:4000/"}),
            json!({"storageKey":"http://127.0.0.1:3000/^0https://other.test"}),
            json!({"storageKey":"null"}),
            json!({}),
        ] {
            let guard = preview_guard();
            assert!(matches!(
                guard.verify_storage_origin(Ok(result)),
                Err(BrowserError::Containment(_))
            ));
            assert_eq!(
                guard.reason(),
                Some("a subframe storage origin was outside the preview origin")
            );
        }
    }

    #[test]
    fn a_refused_storage_key_query_stops_disclosure() {
        let guard = preview_guard();
        let error = BrowserError::Protocol("Storage.getStorageKeyForFrame refused".into());
        assert!(matches!(
            guard.verify_storage_origin(Err(error)),
            Err(BrowserError::Containment(_))
        ));
        assert_eq!(
            guard.reason(),
            Some("Chromium could not verify a subframe storage origin")
        );
    }

    #[test]
    fn main_frame_navigation_to_unsupported_schemes_still_stops_the_preview() {
        for url in [
            "file:///etc/passwd",
            "chrome://settings",
            "javascript:alert(1)",
            "data:text/html,x",
            "about:blank",
        ] {
            let guard = preview_guard();
            guard.state.lock().unwrap().main_frame = Some("main".into());
            let params = json!({"url": url, "frameId": "main"});
            assert!(
                matches!(
                    guard.check_navigation_start(&params),
                    Err(BrowserError::Containment(_))
                ),
                "{url} must stop the preview"
            );
            assert_eq!(
                guard.reason(),
                Some("a main-frame document tried to navigate to an unsupported URL scheme")
            );
        }
    }

    #[test]
    fn the_starting_blank_page_and_preview_pages_may_be_navigated_to() {
        let guard = preview_guard();
        let blank = json!({"url": "about:blank", "frameId": "main"});
        guard.check_navigation_start(&blank).unwrap();
        let page = json!({"url": "http://127.0.0.1:3000/next", "frameId": "main"});
        guard.check_navigation_start(&page).unwrap();
        assert!(guard.reason().is_none());
    }
}
