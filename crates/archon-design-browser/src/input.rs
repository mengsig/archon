//! How forwarded input becomes CDP commands, and what it leaves held down:
//! a batch cut short must not leave a key, button or finger pressed in Chrome.

use serde_json::{Value, json};

use crate::keys::virtual_key_code;
use crate::{Input, KeyAction, MouseAction, MouseButton, TouchAction};

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

/// What has been sent down and not yet up. A press counts as soon as it is
/// sent: Chrome may act on it even when its answer never comes.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Held {
    buttons: Vec<MouseButton>,
    pointer: (f64, f64),
    keys: Vec<(String, String)>,
    touching: bool,
}

impl Held {
    pub(crate) fn note(&mut self, e: &Input) {
        match e {
            Input::Mouse {
                action,
                x,
                y,
                button,
                ..
            } => {
                self.pointer = (*x, *y);
                let at = self.buttons.iter().position(|b| b == button);
                match (action, at) {
                    (MouseAction::Down, None) if *button != MouseButton::None => {
                        self.buttons.push(*button)
                    }
                    (MouseAction::Up, Some(i)) => _ = self.buttons.remove(i),
                    _ => {}
                }
            }
            Input::Wheel { x, y, .. } => self.pointer = (*x, *y),
            Input::Key {
                action, key, code, ..
            } => {
                let at = self.keys.iter().position(|(_, c)| c == code);
                match (action, at) {
                    (KeyAction::Down, None) => self.keys.push((key.clone(), code.clone())),
                    (KeyAction::Up, Some(i)) => _ = self.keys.remove(i),
                    _ => {}
                }
            }
            Input::Touch { action, .. } => {
                self.touching = matches!(action, TouchAction::Start | TouchAction::Move)
            }
            Input::Text { .. } => {}
        }
        debug_assert!(self.buttons.len() <= 3, "at most three buttons are held");
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
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TouchPoint;

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

    #[test]
    fn whatever_was_pressed_and_not_let_go_is_released() {
        let mut held = Held::default();
        for e in [
            mouse(MouseAction::Down, MouseButton::Left),
            key(KeyAction::Down, "ShiftLeft"),
            key(KeyAction::Down, "KeyA"),
            key(KeyAction::Up, "KeyA"),
            Input::Touch {
                action: TouchAction::Start,
                points: vec![TouchPoint {
                    x: 1.0,
                    y: 2.0,
                    id: 0,
                }],
            },
        ] {
            held.note(&e);
        }
        let releases = held.releases();
        assert_eq!(releases.len(), 3);
        assert_eq!(releases[0], mouse(MouseAction::Up, MouseButton::Left));
        assert_eq!(releases[1], key(KeyAction::Up, "ShiftLeft"));
        assert!(
            matches!(&releases[2], Input::Touch { action: TouchAction::Cancel, points } if points.is_empty())
        );
    }

    #[test]
    fn a_finished_gesture_leaves_nothing_held() {
        let mut held = Held::default();
        held.note(&mouse(MouseAction::Down, MouseButton::Right));
        held.note(&mouse(MouseAction::Up, MouseButton::Right));
        held.note(&key(KeyAction::Up, "KeyQ"));
        held.note(&Input::Touch {
            action: TouchAction::End,
            points: Vec::new(),
        });
        assert!(held.releases().is_empty());
        assert_eq!(held.buttons, Vec::new());
    }
}
