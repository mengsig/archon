//! The images attached to the line being typed. Each shows in the line as a
//! chip, `[image #1]`: deleting the chip (one Backspace) drops the image, and
//! the message carries the images whose chips are still there when it is sent.
//! The images stay attached until a message carrying them is sent.

use std::path::Path;

use archon_core::attachment::{
    ImageAttachment, MAX_IMAGES_PER_MESSAGE, MAX_MESSAGE_IMAGE_BYTES, human_size,
};

use crate::clipboard::Clipboard;
use crate::images::{pasted_image_path, read_image_file};

#[derive(Debug, Default)]
pub struct Draft {
    /// Each image with the number its chip shows, in the order attached.
    attached: Vec<(u32, ImageAttachment)>,
    /// The number the last chip got; numbers are not reused within a draft.
    last: u32,
}

/// What one line sends.
#[derive(Debug, PartialEq, Eq)]
pub struct Message {
    pub text: String,
    pub images: Vec<ImageAttachment>,
    /// The attached images whose chips the line does not hold: not sent.
    pub left_out: Vec<u32>,
}

/// What a Ctrl+V does to the line: text to insert and something to say.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pasted {
    pub insert: Option<String>,
    pub notice: Option<String>,
}

pub fn chip(number: u32) -> String {
    format!("[image #{number}]")
}

impl Draft {
    /// Attach `image` to a draft that reads `line` now; an image whose chip
    /// was deleted from it no longer counts. Returns the chip to insert.
    pub fn attach(&mut self, image: ImageAttachment, line: &str) -> Result<String, String> {
        let _dropped = self.keep(line);
        if self.attached.len() >= MAX_IMAGES_PER_MESSAGE {
            return Err(format!(
                "not attached: a message takes at most {MAX_IMAGES_PER_MESSAGE} images"
            ));
        }
        let total: u64 = self
            .images()
            .chain([&image])
            .map(ImageAttachment::size_bytes)
            .sum();
        if total > MAX_MESSAGE_IMAGE_BYTES as u64 {
            return Err(format!(
                "not attached: the images of one message can total {} at most",
                human_size(MAX_MESSAGE_IMAGE_BYTES as u64)
            ));
        }
        self.last += 1;
        self.attached.push((self.last, image));
        assert!(self.attached.len() <= MAX_IMAGES_PER_MESSAGE);
        Ok(chip(self.last))
    }

    /// Every chip still attached, as the start of the next line to type.
    pub fn chips(&self) -> String {
        let chips: Vec<String> = self.attached.iter().map(|(n, _)| chip(*n)).collect();
        chips.join(" ")
    }

    /// The message `line` sends: its text without chips, and the images whose
    /// chips it holds, in line order. The draft keeps every image until
    /// [`Draft::clear`], so a send that fails loses none.
    pub fn message(&self, line: &str) -> Message {
        let mut found: Vec<(usize, u32, &ImageAttachment)> = Vec::new();
        let mut left_out = Vec::new();
        for (number, image) in &self.attached {
            match line.find(&chip(*number)) {
                Some(at) => found.push((at, *number, image)),
                None => left_out.push(*number),
            }
        }
        found.sort_by_key(|(at, _, _)| *at);
        let mut text = line.to_owned();
        for (_, number, _) in &found {
            text = text.replace(&format!("{} ", chip(*number)), "");
            text = text.replace(&chip(*number), "");
        }
        let images: Vec<ImageAttachment> = found
            .into_iter()
            .map(|(_, _, image)| image.clone())
            .collect();
        assert_eq!(images.len() + left_out.len(), self.attached.len());
        Message {
            text: text.trim().to_owned(),
            images,
            left_out,
        }
    }

    /// Forget every image, once the message carrying them was sent.
    pub fn clear(&mut self) {
        self.attached.clear();
        self.last = 0;
    }

    /// Drop the images whose chips `line` no longer holds, as when a line
    /// that sends nothing was entered; the rest stay attached. Returns the
    /// numbers of the images dropped.
    pub fn keep(&mut self, line: &str) -> Vec<u32> {
        let mut dropped = Vec::new();
        self.attached.retain(|(number, _)| {
            let held = line.contains(&chip(*number));
            if !held {
                dropped.push(*number);
            }
            held
        });
        debug_assert!(self.attached.iter().all(|(number, _)| *number <= self.last));
        dropped
    }

    fn images(&self) -> impl Iterator<Item = &ImageAttachment> {
        self.attached.iter().map(|(_, image)| image)
    }
}

/// How many bytes a chip right before the cursor takes, so one Backspace
/// deletes it whole.
pub fn chip_before(before_cursor: &str) -> Option<usize> {
    let open = before_cursor.rfind("[image #")?;
    let number = before_cursor[open + "[image #".len()..].strip_suffix(']')?;
    let numbered = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
    numbered.then(|| before_cursor.len() - open)
}

/// `line` without its chips, as the history keeps it: a recalled chip would
/// name an image the draft no longer holds.
pub fn without_chips(line: &str) -> String {
    let mut kept = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find("[image #") {
        kept.push_str(&rest[..open]);
        let after = &rest[open..];
        let chip_width = after
            .find(']')
            .filter(|close| chip_before(&after[..=*close]).is_some());
        rest = match chip_width {
            Some(close) => after[close + 1..]
                .strip_prefix(' ')
                .unwrap_or(&after[close + 1..]),
            None => {
                kept.push('[');
                &after[1..]
            }
        };
    }
    kept.push_str(rest);
    kept.trim().to_owned()
}

/// What Ctrl+V does with `content` while the line reads `line`: an image is
/// attached; text naming one image file attaches that file; other text is
/// inserted as it is.
pub fn paste(
    draft: &mut Draft,
    line: &str,
    content: Result<Clipboard, String>,
    home: Option<&Path>,
) -> Pasted {
    let attach = |draft: &mut Draft, image: ImageAttachment| match draft.attach(image, line) {
        Ok(chip) => Pasted {
            insert: Some(format!("{chip} ")),
            notice: None,
        },
        Err(refused) => Pasted {
            insert: None,
            notice: Some(refused),
        },
    };
    let text = match content {
        Ok(Clipboard::Image(image)) => return attach(draft, image),
        Ok(Clipboard::Text(text)) => text,
        Err(reason) => {
            return Pasted {
                insert: None,
                notice: Some(format!("nothing pasted: {reason}")),
            };
        }
    };
    let Some(path) = pasted_image_path(&text, home) else {
        return Pasted {
            insert: Some(text),
            notice: None,
        };
    };
    match read_image_file(&path) {
        Ok(image) => attach(draft, image),
        Err(reason) => Pasted {
            insert: Some(text),
            notice: Some(format!("pasted as text, not attached: {reason}")),
        },
    }
}

#[cfg(test)]
mod tests;
