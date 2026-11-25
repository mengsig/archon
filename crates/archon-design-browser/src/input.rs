//! How forwarded input becomes CDP commands, and who owns what it leaves held.
//! This driver is the one owner of a session's held input: a batch that fails,
//! times out or is refused marks input lost, and everything held is released
//! before the next batch or by the daemon's idle cleanup, so nothing sticks.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::browser::{Browser, Shared};
use crate::keys::virtual_key_code;
use crate::{BrowserError, Input, KeyAction, MouseAction, MouseButton, TouchAction};

/// One input batch, from waiting for the batch before it to its last event:
/// with the releases a lost one owes, inside the 60 s the web app waits.
pub(crate) const INPUT_DEADLINE: Duration = Duration::from_secs(8);
/// Each release of held input is answered within this, or tried again later.
const RELEASE_DEADLINE: Duration = Duration::from_secs(2);

/// The CDP method and parameters that dispatch `e`.
pub(crate) fn command(e: &Input) -> (&'static str, Value) {
    match e {
        Input::Mouse {
            action,
            x,
            y,
            button,
            click_count,
            modifiers,
        } => (
            "Input.dispatchMouseEvent",
            json!({
                "type": match action { MouseAction::Move => "mouseMoved", MouseAction::Down => "mousePressed", MouseAction::Up => "mouseReleased" },
                "x": x, "y": y, "button": button_name(*button), "clickCount": click_count, "modifiers": modifiers,
            }),
        ),
        Input::Wheel {
            x,
            y,
            dx,
            dy,
            modifiers,
        } => (
            "Input.dispatchMouseEvent",
            json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy, "modifiers": modifiers}),
        ),
        Input::Key {
            action,
            key,
            code,
            text,
            modifiers,
        } => (
            "Input.dispatchKeyEvent",
            key_params(*action, key, code, text.as_deref(), *modifiers),
        ),
        Input::Text { text } => ("Input.insertText", json!({"text": text})),
        Input::Touch { action, points } => (
            "Input.dispatchTouchEvent",
            json!({
                "type": match action { TouchAction::Start => "touchStart", TouchAction::Move => "touchMove", TouchAction::End => "touchEnd", TouchAction::Cancel => "touchCancel" },
                "touchPoints": points.iter().map(|p| json!({"x": p.x, "y": p.y, "id": p.id})).collect::<Vec<_>>(),
            }),
        ),
    }
}

fn key_params(
    action: KeyAction,
    key: &str,
    code: &str,
    text: Option<&str>,
    modifiers: u8,
) -> Value {
    let ty = match (action, text.is_some_and(|t| !t.is_empty())) {
        (KeyAction::Up, _) => "keyUp",
        (KeyAction::Down, true) => "keyDown",
        (KeyAction::Down, false) => "rawKeyDown",
    };
    let mut p = json!({"type": ty, "key": key, "code": code, "modifiers": modifiers});
    // Chrome scrolls, deletes and tabs by the key code, not the key name.
    if let Some(vk) = virtual_key_code(key, code) {
        p["windowsVirtualKeyCode"] = json!(vk);
        p["nativeVirtualKeyCode"] = json!(vk);
    }
    if let (KeyAction::Down, Some(t)) = (action, text) {
        p["text"] = json!(t);
    }
    p
}

pub(crate) fn button_name(b: MouseButton) -> &'static str {
    match b {
        MouseButton::None => "none",
        MouseButton::Left => "left",
        MouseButton::Middle => "middle",
        MouseButton::Right => "right",
    }
}

/// What may be pressed in Chrome: a press counts once it is sent (Chrome
/// may act on it even when no answer comes), a release only once answered.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Held {
    buttons: Vec<MouseButton>,
    pointer: (f64, f64),
    /// `(key, code)`; one entry however often the key repeats.
    keys: Vec<(String, String)>,
    touching: bool,
}

/// A key's identity: its code, or its name where the device gives no code.
fn key_id<'a>(key: &'a str, code: &'a str) -> &'a str {
    if code.is_empty() { key } else { code }
}

impl Held {
    /// `e` is about to be sent: what it presses counts as held from now on.
    pub(crate) fn pressing(&mut self, e: &Input) {
        match e {
            Input::Mouse {
                action,
                x,
                y,
                button,
                ..
            } => {
                self.pointer = (*x, *y);
                let pressed = *action == MouseAction::Down && *button != MouseButton::None;
                if pressed && !self.buttons.contains(button) {
                    self.buttons.push(*button);
                }
            }
            Input::Wheel { x, y, .. } => self.pointer = (*x, *y),
            Input::Key {
                action: KeyAction::Down,
                key,
                code,
                ..
            } => {
                let id = key_id(key, code);
                if !self.keys.iter().any(|(k, c)| key_id(k, c) == id) {
                    self.keys.push((key.clone(), code.clone()));
                }
            }
            Input::Touch {
                action: TouchAction::Start | TouchAction::Move,
                ..
            } => self.touching = true,
            Input::Key { .. } | Input::Touch { .. } | Input::Text { .. } => {}
        }
        debug_assert!(self.buttons.len() <= 3, "at most three buttons are held");
    }

    /// Chrome answered `e`: what it releases is no longer held.
    pub(crate) fn acknowledged(&mut self, e: &Input) {
        let before = self.buttons.len() + self.keys.len();
        match e {
            Input::Mouse {
                action: MouseAction::Up,
                button,
                ..
            } => self.buttons.retain(|b| b != button),
            Input::Key {
                action: KeyAction::Up,
                key,
                code,
                ..
            } => {
                let id = key_id(key, code);
                self.keys.retain(|(k, c)| key_id(k, c) != id);
            }
            Input::Touch {
                action: TouchAction::End | TouchAction::Cancel,
                ..
            } => self.touching = false,
            _ => {}
        }
        assert!(
            self.buttons.len() + self.keys.len() <= before,
            "an answer only lets go"
        );
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buttons.is_empty() && self.keys.is_empty() && !self.touching
    }

    /// Releases for everything held, in the order a person would let go.
    pub(crate) fn releases(&self) -> Vec<Input> {
        let (x, y) = self.pointer;
        let buttons = self.buttons.iter().map(|&button| Input::Mouse {
            action: MouseAction::Up,
            x,
            y,
            button,
            click_count: 1,
            modifiers: 0,
        });
        let keys = self.keys.iter().rev().map(|(key, code)| Input::Key {
            action: KeyAction::Up,
            key: key.clone(),
            code: code.clone(),
            text: None,
            modifiers: 0,
        });
        let touch = self.touching.then_some(Input::Touch {
            action: TouchAction::Cancel,
            points: Vec::new(),
        });
        let all: Vec<Input> = buttons.chain(keys).chain(touch).collect();
        assert!(all.len() <= self.buttons.len() + self.keys.len() + 1);
        assert_eq!(all.is_empty(), self.is_empty());
        all
    }
}

impl Browser {
    /// Dispatch `events` in order within [`INPUT_DEADLINE`], first letting go
    /// of whatever a lost batch left held. A failure marks this batch lost.
    pub async fn input(&self, events: &[Input]) -> Result<(), BrowserError> {
        let sh = &self.sh;
        let mut outcome = LostUnlessDone { sh, done: false };
        let deadline = Instant::now() + INPUT_DEADLINE;
        let Ok(mut held) = tokio::time::timeout_at(deadline, sh.input.lock()).await else {
            return Err(BrowserError::Timeout(format!(
                "earlier input still ran after {} s; this did not reach the app",
                INPUT_DEADLINE.as_secs()
            )));
        };
        sh.input_started();
        if sh.input_lost.swap(false, Ordering::SeqCst) {
            release_held(sh, &mut held).await;
        }
        let dispatch = async {
            for e in events {
                held.pressing(e);
                let (method, params) = command(e);
                sh.call(method, params).await?;
                held.acknowledged(e);
            }
            Ok(())
        };
        let result = tokio::time::timeout_at(deadline, dispatch)
            .await
            .unwrap_or_else(|_| {
                Err(BrowserError::Timeout(format!(
                    "input took over {} s; some of it may not have reached the app",
                    INPUT_DEADLINE.as_secs()
                )))
            });
        outcome.done = result.is_ok();
        result
    }

    /// A batch for this page failed before it reached Chrome: whatever it
    /// would have let go is released before the next, or by [`Self::reconcile_input`].
    pub fn input_lost(&self) {
        self.sh.input_lost.store(true, Ordering::SeqCst);
    }

    /// The idle cleanup: after a lost batch, let go of everything still held.
    /// True when nothing a lost batch may have left is held any more.
    pub async fn reconcile_input(&self) -> bool {
        let sh = &self.sh;
        if !sh.input_lost.load(Ordering::SeqCst) {
            return true;
        }
        // A batch running now reconciles before it dispatches.
        let Ok(mut held) = sh.input.try_lock() else {
            return false;
        };
        if !sh.input_lost.swap(false, Ordering::SeqCst) {
            return true;
        }
        let mut outcome = LostUnlessDone { sh, done: false };
        // An unanswered release marks input lost again itself.
        let released = release_held(sh, &mut held).await;
        outcome.done = true;
        released
    }
}

/// Marks input lost when dropped before `done`: a batch that fails, times out
/// or is cancelled (its request dropped mid-dispatch) may have left a press held.
struct LostUnlessDone<'a> {
    sh: &'a Shared,
    done: bool,
}

impl Drop for LostUnlessDone<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.sh.input_lost.store(true, Ordering::SeqCst);
        }
    }
}

/// Send a release for every held entry at once, each within its own
/// [`RELEASE_DEADLINE`], so one Chrome leaves unanswered holds up none of the
/// others. One unanswered stays held and marks input lost again; true when none is.
async fn release_held(sh: &Arc<Shared>, held: &mut Held) -> bool {
    let releases = held.releases();
    let mut sent = tokio::task::JoinSet::new();
    for release in releases {
        let sh = sh.clone();
        sent.spawn(async move {
            let (method, params) = command(&release);
            let answer = tokio::time::timeout(RELEASE_DEADLINE, sh.call(method, params)).await;
            (release, matches!(answer, Ok(Ok(_))))
        });
    }
    let mut unanswered = 0usize;
    while let Some(done) = sent.join_next().await {
        match done {
            Ok((release, true)) => held.acknowledged(&release),
            Ok((_, false)) => unanswered += 1,
            Err(error) => {
                tracing::warn!(%error, "a preview input release did not finish");
                unanswered += 1;
            }
        }
    }
    if unanswered == 0 {
        return true;
    }
    tracing::warn!(
        unanswered,
        "held preview input was not released; it is tried again"
    );
    sh.input_lost.store(true, Ordering::SeqCst);
    debug_assert!(!held.is_empty(), "an unanswered release is still held");
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TouchPoint;
    use crate::testutil::{Log, fake_browser, no_answer};

    fn mouse(action: MouseAction, button: MouseButton) -> Input {
        Input::Mouse {
            action,
            x: 7.0,
            y: 9.0,
            button,
            click_count: 1,
            modifiers: 0,
        }
    }
    fn key(action: KeyAction, code: &str) -> Input {
        Input::Key {
            action,
            key: code.to_lowercase(),
            code: code.into(),
            text: None,
            modifiers: 0,
        }
    }
    fn touch(action: TouchAction) -> Input {
        let points = match action {
            TouchAction::Start | TouchAction::Move => vec![TouchPoint {
                x: 1.0,
                y: 2.0,
                id: 0,
            }],
            TouchAction::End | TouchAction::Cancel => Vec::new(),
        };
        Input::Touch { action, points }
    }
    fn sent_since(log: &Log, from: usize) -> Vec<String> {
        log.lock().unwrap()[from..]
            .iter()
            .filter(|(m, _)| m.starts_with("Input."))
            .map(|(_, p)| {
                format!(
                    "{} {}",
                    p["type"].as_str().unwrap_or(""),
                    p["code"].as_str().or(p["button"].as_str()).unwrap_or("")
                )
                .trim()
                .to_owned()
            })
            .collect()
    }
    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    #[test]
    fn a_press_is_held_from_sending_and_a_release_only_once_answered() {
        let mut held = Held::default();
        for e in [
            mouse(MouseAction::Down, MouseButton::Left),
            key(KeyAction::Down, "ShiftLeft"),
            key(KeyAction::Down, "KeyA"),
            touch(TouchAction::Start),
        ] {
            held.pressing(&e);
        }
        // Sent, never answered: a release changes nothing until Chrome answers it.
        held.pressing(&key(KeyAction::Up, "KeyA"));
        assert_eq!(held.releases().len(), 4);
        held.acknowledged(&key(KeyAction::Up, "KeyA"));
        let releases = held.releases();
        assert_eq!(releases.len(), 3);
        assert_eq!(releases[0], mouse(MouseAction::Up, MouseButton::Left));
        assert_eq!(releases[1], key(KeyAction::Up, "ShiftLeft"));
        assert_eq!(releases[2], touch(TouchAction::Cancel));
    }

    #[test]
    fn a_finished_gesture_leaves_nothing_held() {
        let mut held = Held::default();
        for e in [
            mouse(MouseAction::Down, MouseButton::Right),
            mouse(MouseAction::Up, MouseButton::Right),
            key(KeyAction::Up, "KeyQ"),
            touch(TouchAction::Start),
            touch(TouchAction::End),
        ] {
            held.pressing(&e);
            held.acknowledged(&e);
        }
        assert!(held.is_empty());
        assert!(held.releases().is_empty());
    }

    /// A press whose answer never comes may still have reached Chrome: the
    /// batch fails, and the next batch first lets go of it.
    #[tokio::test(start_paused = true)]
    async fn a_press_that_times_out_is_released_before_the_next_batch() {
        let (b, log) = fake_browser(|m, p| {
            (m == "Input.dispatchKeyEvent" && p["type"] == "rawKeyDown").then(no_answer)
        })
        .await;
        let start = log.lock().unwrap().len();
        let events = [
            mouse(MouseAction::Down, MouseButton::Left),
            key(KeyAction::Down, "KeyB"),
            key(KeyAction::Up, "KeyB"),
            mouse(MouseAction::Up, MouseButton::Left),
        ];
        let lost = b.input(&events).await;
        assert!(
            matches!(&lost, Err(BrowserError::Timeout(m)) if m.contains("may not have reached")),
            "{lost:?}"
        );
        assert_eq!(
            sent_since(&log, start),
            ["mousePressed left", "rawKeyDown KeyB"]
        );
        let next = log.lock().unwrap().len();
        b.input(&[mouse(MouseAction::Move, MouseButton::None)])
            .await
            .expect("input goes on after a lost batch");
        let sent = sent_since(&log, next);
        assert_eq!(
            sorted(sent[..2].to_vec()),
            ["keyUp KeyB", "mouseReleased left"]
        );
        assert_eq!(sent[2..], ["mouseMoved none"]);
        assert!(b.sh.input.lock().await.is_empty());
    }

    /// A release whose CDP call stalls, unsent or unanswered (to the driver
    /// both are a call that never ends), stays held and is sent again.
    #[tokio::test(start_paused = true)]
    async fn a_release_that_stalls_is_sent_again_before_the_next_batch() {
        let (b, log) = fake_browser(|m, p| {
            (m == "Input.dispatchMouseEvent" && p["type"] == "mouseReleased" && p["x"] == 7.0)
                .then(no_answer)
        })
        .await;
        b.input(&[mouse(MouseAction::Down, MouseButton::Left)])
            .await
            .expect("the press is answered");
        let lost = b.input(&[mouse(MouseAction::Up, MouseButton::Left)]).await;
        assert!(matches!(lost, Err(BrowserError::Timeout(_))), "{lost:?}");
        assert_eq!(b.sh.input.lock().await.buttons, [MouseButton::Left]);
        let next = log.lock().unwrap().len();
        let moved = Input::Wheel {
            x: 20.0,
            y: 30.0,
            dx: 0.0,
            dy: 40.0,
            modifiers: 0,
        };
        b.input(&[moved]).await.expect("the wheel is answered");
        // The release goes again at the pointer's last place (7, 9), still unanswered.
        assert_eq!(sent_since(&log, next), ["mouseReleased left", "mouseWheel"]);
        assert!(b.sh.input_lost.load(Ordering::SeqCst), "still owed");
    }

    /// The first release never answered must not keep the others from going.
    #[tokio::test(start_paused = true)]
    async fn an_unanswered_release_holds_up_none_of_the_others() {
        let (b, log) = fake_browser(|m, p| {
            (m == "Input.dispatchMouseEvent" && p["type"] == "mouseReleased").then(no_answer)
        })
        .await;
        let presses = [
            mouse(MouseAction::Down, MouseButton::Left),
            key(KeyAction::Down, "ShiftLeft"),
            key(KeyAction::Down, "KeyC"),
        ];
        b.input(&presses).await.expect("presses are answered");
        b.input_lost();
        let started = Instant::now();
        let next = log.lock().unwrap().len();
        assert!(!b.reconcile_input().await, "the button is still owed");
        assert!(
            started.elapsed() <= RELEASE_DEADLINE,
            "releases wait side by side"
        );
        assert_eq!(
            sorted(sent_since(&log, next)),
            ["keyUp KeyC", "keyUp ShiftLeft", "mouseReleased left"]
        );
        let held = b.sh.input.lock().await;
        assert_eq!(
            (held.buttons.as_slice(), held.keys.len()),
            ([MouseButton::Left].as_slice(), 0)
        );
    }

    /// Auto-repeat sends a key down many times; it is one held key, and one
    /// release lets go of it after its batch was lost.
    #[tokio::test]
    async fn a_repeating_key_is_released_once_after_a_lost_batch() {
        let (b, log) = fake_browser(|_, _| None).await;
        let repeats = vec![key(KeyAction::Down, "ArrowDown"); 5];
        b.input(&repeats).await.expect("repeats are answered");
        assert_eq!(b.sh.input.lock().await.keys.len(), 1);
        b.input_lost();
        let next = log.lock().unwrap().len();
        assert!(b.reconcile_input().await);
        assert_eq!(sent_since(&log, next), ["keyUp ArrowDown"]);
        assert!(b.reconcile_input().await, "nothing more is owed");
        assert_eq!(sent_since(&log, next).len(), 1);
    }

    /// A request dropped mid-batch (its connection closed) leaves a press it
    /// sent behind: the next batch lets go of it all the same.
    #[tokio::test(start_paused = true)]
    async fn a_batch_cancelled_after_its_press_is_released_before_the_next() {
        let (b, log) = fake_browser(|m, p| {
            (m == "Input.dispatchKeyEvent" && p["type"] == "keyUp").then(no_answer)
        })
        .await;
        let gesture = [key(KeyAction::Down, "KeyD"), key(KeyAction::Up, "KeyD")];
        let cut = tokio::time::timeout(Duration::from_secs(1), b.input(&gesture)).await;
        assert!(cut.is_err(), "the batch was still waiting on its release");
        assert!(b.sh.input_lost.load(Ordering::SeqCst));
        let next = log.lock().unwrap().len();
        assert!(!b.reconcile_input().await, "the fake never answers a keyUp");
        assert_eq!(sent_since(&log, next), ["keyUp KeyD"]);
    }

    /// Batches that end well hold a press across them (a drag) untouched.
    #[tokio::test]
    async fn a_press_held_across_good_batches_is_not_released() {
        let (b, log) = fake_browser(|_, _| None).await;
        b.input(&[mouse(MouseAction::Down, MouseButton::Left)])
            .await
            .unwrap();
        b.input(&[mouse(MouseAction::Move, MouseButton::Left)])
            .await
            .unwrap();
        assert!(b.reconcile_input().await);
        assert_eq!(
            sent_since(&log, 0),
            ["mousePressed left", "mouseMoved left"]
        );
        assert_eq!(b.sh.input.lock().await.buttons, [MouseButton::Left]);
    }
}
