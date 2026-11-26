//! Windows virtual key codes for forwarded keys. Chrome acts on a key's code,
//! not its name: without one, PageDown, the arrows, Home, End, Space,
//! Backspace and Tab reached the page as events but did nothing.

/// The virtual key code of a DOM `key` and `code` pair, when it has one.
pub(crate) fn virtual_key_code(key: &str, code: &str) -> Option<u32> {
    if let Some(vk) = named(key) {
        return Some(vk);
    }
    let mut chars = key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return by_code(code);
    };
    match c.to_ascii_uppercase() {
        upper @ ('A'..='Z' | '0'..='9') => Some(u32::from(upper)),
        ' ' => Some(0x20),
        _ => by_code(code),
    }
}

fn named(key: &str) -> Option<u32> {
    Some(match key {
        "Backspace" => 0x08,
        "Tab" => 0x09,
        "Enter" => 0x0D,
        "Shift" => 0x10,
        "Control" => 0x11,
        "Alt" => 0x12,
        "Escape" => 0x1B,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "End" => 0x23,
        "Home" => 0x24,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "Insert" => 0x2D,
        "Delete" => 0x2E,
        "Meta" => 0x5B,
        _ => return function_key(key),
    })
}

fn function_key(key: &str) -> Option<u32> {
    let n: u32 = key.strip_prefix('F')?.parse().ok()?;
    (1..=12).contains(&n).then(|| 0x70 + n - 1)
}

/// Punctuation and shifted digits: the physical key decides.
fn by_code(code: &str) -> Option<u32> {
    if let Some(digit) = code.strip_prefix("Digit") {
        let mut chars = digit.chars();
        if let (Some(c @ '0'..='9'), None) = (chars.next(), chars.next()) {
            return Some(u32::from(c));
        }
    }
    Some(match code {
        "Space" => 0x20,
        "Semicolon" => 0xBA,
        "Equal" => 0xBB,
        "Comma" => 0xBC,
        "Minus" => 0xBD,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "Backquote" => 0xC0,
        "BracketLeft" => 0xDB,
        "Backslash" => 0xDC,
        "BracketRight" => 0xDD,
        "Quote" => 0xDE,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::virtual_key_code;

    #[test]
    fn scrolling_keys_have_their_chrome_codes() {
        let codes: Vec<_> = ["PageUp", "PageDown", "End", "Home", "ArrowUp", "ArrowDown"]
            .iter()
            .map(|k| virtual_key_code(k, k))
            .collect();
        assert_eq!(
            codes,
            [Some(33), Some(34), Some(35), Some(36), Some(38), Some(40)]
        );
        assert_eq!(virtual_key_code(" ", "Space"), Some(32));
    }

    #[test]
    fn letters_digits_and_punctuation_follow_the_physical_key() {
        assert_eq!(virtual_key_code("a", "KeyA"), Some(65));
        assert_eq!(virtual_key_code("Z", "KeyZ"), Some(90));
        assert_eq!(virtual_key_code("7", "Digit7"), Some(55));
        assert_eq!(virtual_key_code("!", "Digit1"), Some(49));
        assert_eq!(virtual_key_code("/", "Slash"), Some(0xBF));
        assert_eq!(virtual_key_code("F5", "F5"), Some(0x74));
        assert_eq!(virtual_key_code("F13", "F13"), None);
        assert_eq!(virtual_key_code("é", ""), None);
        assert_eq!(virtual_key_code("Dead", "Quote"), Some(0xDE));
    }
}
