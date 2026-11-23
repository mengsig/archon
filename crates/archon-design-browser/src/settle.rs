//! Taking a picture only once the page has settled, and the pointer targets
//! and picture comparison a design state needs: a reload that has finished
//! loading, a page whose pixels stopped moving, the centre of an element
//! scrolled into view, and whether two pictures show the same thing.

use std::time::Duration;

use tokio::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::json;

use crate::browser::Browser;
use crate::{BrowserError, BrowserEvent};

/// Longest side, in pixels, pictures are compared at.
const COMPARE_SIDE: u32 = 1024;
/// A pixel differs when a colour channel moves by more than this (of 255):
/// above an encoder's noise, below a shade of blue a button darkens by.
const CHANNEL_TOLERANCE: u32 = 8;
/// Pictures match when no more than this share of their pixels differ: a
/// couple of dozen pixels at the compare size, below a 1 px border or a label.
const MATCH_SHARE: f64 = 0.00002;
/// How often the page is looked at while it settles.
const SETTLE_POLL: Duration = Duration::from_millis(200);
/// How long the network must be quiet for the page to count as settled.
const SETTLE_QUIET: Duration = Duration::from_millis(400);

/// Draws both pictures at one size and counts the pixels that differ,
/// returning their share, or 1 when the proportions differ. Runs in the page
/// on bytes it is given: nothing is fetched, so a page's content security
/// policy cannot stop it.
const COMPARE_JS: &str = r#"(async (a, b, side, tol) => {
  const load = async (p) => {
    const bin = atob(p.d); const u8 = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
    return createImageBitmap(new Blob([u8], {type: p.t}));
  };
  const [x, y] = await Promise.all([load(a), load(b)]);
  const ra = x.width / x.height, rb = y.width / y.height;
  if (Math.abs(ra - rb) > 0.02 * Math.max(ra, rb)) return 1;
  const w = Math.max(1, Math.min(side, x.width, y.width));
  const h = Math.max(1, Math.round(w / ra));
  const px = (img) => {
    const c = new OffscreenCanvas(w, h); const g = c.getContext('2d');
    g.imageSmoothingQuality = 'high'; g.drawImage(img, 0, 0, w, h);
    return g.getImageData(0, 0, w, h).data;
  };
  const p = px(x), q = px(y); let n = 0;
  for (let i = 0; i < p.length; i += 4) {
    if (Math.max(Math.abs(p[i] - q[i]), Math.abs(p[i + 1] - q[i + 1]), Math.abs(p[i + 2] - q[i + 2])) > tol) n++;
  }
  return n / (w * h);
})"#;

impl Browser {
    /// Reload and wait until the page has loaded, then settle. `false` when
    /// `max` passed first.
    pub async fn reload_settled(&self, max: Duration) -> Result<bool, BrowserError> {
        let mut events = self.events();
        self.reload().await?;
        let loaded = wait_loaded(&mut events, max).await;
        Ok(self.wait_settled(max).await? && loaded)
    }

    /// Navigate, wait until the page has loaded, then settle.
    pub async fn navigate_settled(&self, url: &str, max: Duration) -> Result<bool, BrowserError> {
        let mut events = self.events();
        self.navigate(url).await?;
        let loaded = wait_loaded(&mut events, max).await;
        Ok(self.wait_settled(max).await? && loaded)
    }

    /// Wait until no request is in flight and two looks at the page, a
    /// moment apart, show the same pixels (no reload, hot update or layout
    /// shift still moving it). `false` when `max` passed first.
    pub async fn wait_settled(&self, max: Duration) -> Result<bool, BrowserError> {
        let deadline = Instant::now() + max;
        let mut last: Option<Vec<u8>> = None;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let quiet = self.wait_network_idle(SETTLE_QUIET, left).await?;
            let look = self.sh.capture(None, 0.25).await?.bytes;
            if quiet && last.as_deref() == Some(look.as_slice()) {
                return Ok(true);
            }
            last = Some(look);
            if Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(SETTLE_POLL).await;
        }
    }

    /// The centre of the element `selector` matches, in CSS px, after
    /// scrolling it into view; `None` when nothing matches or it has no box.
    pub async fn center_of(&self, selector: &str) -> Result<Option<(f64, f64)>, BrowserError> {
        let doc = self.sh.call("DOM.getDocument", json!({"depth": 0})).await?;
        let root = doc["root"]["nodeId"].as_i64().unwrap_or(1);
        let found = match self
            .sh
            .call(
                "DOM.querySelector",
                json!({"nodeId": root, "selector": selector}),
            )
            .await
        {
            Ok(found) => found,
            Err(BrowserError::Protocol(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let node = found["nodeId"].as_i64().unwrap_or(0);
        if node == 0 {
            return Ok(None);
        }
        // Best effort: an element already in view needs no scrolling.
        let _ = self
            .sh
            .call("DOM.scrollIntoViewIfNeeded", json!({"nodeId": node}))
            .await;
        Ok(self
            .resolve_selector(selector, None)
            .await?
            .map(|r| (r.x + r.w / 2.0, r.y + r.h / 2.0)))
    }

    /// Whether two pictures show the same thing: the same proportions and, at
    /// one common size, no more than a thousandth of their pixels differing
    /// visibly. The browser decodes them (a PNG and a JPEG compare), so an
    /// encoder's noise or a different scale does not count as a change.
    pub async fn pictures_match(
        &self,
        a: (&str, &[u8]),
        b: (&str, &[u8]),
    ) -> Result<bool, BrowserError> {
        let pic = |(mime, bytes): (&str, &[u8])| json!({"t": mime, "d": B64.encode(bytes)});
        let expression = format!(
            "{COMPARE_JS}({}, {}, {COMPARE_SIDE}, {CHANNEL_TOLERANCE})",
            pic(a),
            pic(b)
        );
        let r = self
            .sh
            .call(
                "Runtime.evaluate",
                json!({"expression": expression, "awaitPromise": true, "returnByValue": true}),
            )
            .await?;
        if let Some(why) = r["exceptionDetails"]["exception"]["description"]
            .as_str()
            .or_else(|| r["exceptionDetails"]["text"].as_str())
        {
            return Err(BrowserError::Protocol(format!(
                "comparing pictures failed: {why}"
            )));
        }
        let share = r["result"]["value"]
            .as_f64()
            .ok_or_else(|| BrowserError::Protocol("comparing pictures gave no number".into()))?;
        Ok(share <= MATCH_SHARE)
    }
}

/// Wait for the page's load event, a failed navigation, or `max`.
async fn wait_loaded(
    events: &mut tokio::sync::broadcast::Receiver<BrowserEvent>,
    max: Duration,
) -> bool {
    let wait = async {
        loop {
            match events.recv().await {
                Ok(BrowserEvent::Loaded { .. }) => return true,
                Ok(BrowserEvent::NavigationFailed { .. } | BrowserEvent::Crashed) => return false,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
            }
        }
    };
    tokio::time::timeout(max, wait).await.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use serde_json::Value;

    use crate::testutil::{fake_browser, fake_browser_loading};

    fn shot(bytes: &[u8]) -> Value {
        json!({"data": B64.encode(bytes)})
    }

    #[tokio::test]
    async fn a_page_still_moving_is_not_settled_until_two_looks_match() {
        let looks = std::sync::Arc::new(AtomicUsize::new(0));
        let seen = looks.clone();
        let (b, _log) = fake_browser(move |m, _| {
            (m == "Page.captureScreenshot").then(|| {
                // A hot update still redrawing: three different pictures, then rest.
                let n = seen.fetch_add(1, Ordering::SeqCst);
                shot(&[1, 2, 3, n.min(3) as u8])
            })
        })
        .await;
        let settled = b.wait_settled(Duration::from_secs(10)).await.unwrap();
        assert!(settled, "it settled once the pictures stopped changing");
        assert!(
            looks.load(Ordering::SeqCst) >= 5,
            "it looked until two in a row matched, not once: {}",
            looks.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn a_page_that_never_rests_is_given_up_on_at_the_limit() {
        let looks = std::sync::Arc::new(AtomicUsize::new(0));
        let seen = looks.clone();
        let (b, _log) = fake_browser(move |m, _| {
            (m == "Page.captureScreenshot")
                .then(|| shot(&[seen.fetch_add(1, Ordering::SeqCst) as u8]))
        })
        .await;
        let settled = b.wait_settled(Duration::from_millis(900)).await.unwrap();
        assert!(!settled, "an animation that never stops does not settle");
    }

    #[tokio::test]
    async fn a_reload_is_waited_for_until_the_page_has_loaded() {
        let (b, log) = fake_browser_loading(
            |m, _| (m == "Page.captureScreenshot").then(|| shot(&[9, 9])),
            Duration::from_millis(600),
        )
        .await;
        let started = std::time::Instant::now();
        assert!(b.reload_settled(Duration::from_secs(10)).await.unwrap());
        assert!(
            started.elapsed() >= Duration::from_millis(600),
            "returned before the load event: {:?}",
            started.elapsed()
        );
        assert!(log.lock().unwrap().iter().any(|(m, _)| m == "Page.reload"));
    }

    #[tokio::test]
    async fn the_centre_of_an_element_is_scrolled_to_first() {
        let (b, log) = fake_browser(|m, _| match m {
            "DOM.getDocument" => Some(json!({"root": {"nodeId": 1}})),
            "DOM.querySelector" => Some(json!({"nodeId": 7})),
            "DOM.querySelectorAll" => Some(json!({"nodeIds": [7]})),
            "DOM.getBoxModel" => {
                Some(json!({"model": {"border": [10, 20, 110, 20, 110, 60, 10, 60]}}))
            }
            _ => None,
        })
        .await;
        let at = b.center_of("button.new").await.unwrap();
        assert_eq!(at, Some((60.0, 40.0)));
        let calls: Vec<String> = log.lock().unwrap().iter().map(|(m, _)| m.clone()).collect();
        let scrolled = calls.iter().position(|m| m == "DOM.scrollIntoViewIfNeeded");
        let boxed = calls.iter().position(|m| m == "DOM.getBoxModel");
        assert!(scrolled.is_some() && scrolled < boxed, "{calls:?}");
        let (none, _) = fake_browser(|m, _| match m {
            "DOM.getDocument" => Some(json!({"root": {"nodeId": 1}})),
            "DOM.querySelector" => Some(json!({"nodeId": 0})),
            _ => None,
        })
        .await;
        assert_eq!(none.center_of("nothing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn pictures_are_compared_in_the_page_on_the_bytes_given() {
        let (b, log) = fake_browser(|m, _| {
            (m == "Runtime.evaluate").then(|| json!({"result": {"value": 0.00001}}))
        })
        .await;
        assert!(
            b.pictures_match(("image/jpeg", &[1, 2]), ("image/png", &[3, 4]))
                .await
                .unwrap()
        );
        {
            let log = log.lock().unwrap();
            let (_, params) = log.iter().find(|(m, _)| m == "Runtime.evaluate").unwrap();
            let expression = params["expression"].as_str().unwrap();
            assert!(expression.contains(&B64.encode([1, 2])) && expression.contains("image/png"));
            assert_eq!(params["awaitPromise"], true);
        }
        let (differs, _) = fake_browser(|m, _| {
            (m == "Runtime.evaluate").then(|| json!({"result": {"value": 0.2}}))
        })
        .await;
        assert!(
            !differs
                .pictures_match(("image/jpeg", &[1]), ("image/jpeg", &[2]))
                .await
                .unwrap()
        );
        let (broken, _) = fake_browser(|m, _| {
            (m == "Runtime.evaluate")
                .then(|| json!({"exceptionDetails": {"text": "Uncaught", "exception": {"description": "EncodingError"}}}))
        })
        .await;
        assert!(
            broken
                .pictures_match(("image/jpeg", &[1]), ("image/jpeg", &[2]))
                .await
                .is_err()
        );
    }

    /// A shade of blue a button darkens by (blue-500 to blue-600) moves each
    /// channel by more than the tolerance, so it is a change, not noise.
    #[test]
    fn a_darker_shade_is_a_difference_not_noise() {
        let (a, b) = ([59_i32, 130, 246], [37_i32, 99, 235]);
        assert!(
            a.iter()
                .zip(b)
                .all(|(x, y)| x.abs_diff(y) > super::CHANNEL_TOLERANCE)
        );
    }
}
