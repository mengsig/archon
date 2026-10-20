//! Images the operator sends with a chat message. They reach the orchestrator
//! as Aether's `send_message.attachments` (Aether protocol 19), so the limits
//! and checks here are Aether's own (`aether_core::attachment`): archond
//! refuses what aetherd would, before megabytes cross a second socket.
//!
//! The bytes travel base64-encoded in the request; the feed keeps only an
//! [`ImageRef`] naming each image, and Aether stores the bytes.

use std::fmt;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Largest single image, in bytes (3.75 MiB), as Aether takes it.
pub const MAX_IMAGE_BYTES: usize = 3_932_160;

/// Most images one message may carry.
pub const MAX_IMAGES_PER_MESSAGE: usize = 10;

/// Most image bytes one message may carry in total (10 MiB).
pub const MAX_MESSAGE_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Longest file name kept with an image; a longer one is cut.
pub const MAX_ATTACHMENT_NAME_CHARS: usize = 120;

/// One image as a client sends it: Aether's `ImageAttachment`, field for field.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAttachment {
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`, checked
    /// against the bytes.
    pub media_type: String,
    /// The image bytes, base64 (standard alphabet, padded).
    pub data: String,
    /// The file name the image had, when the client knows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ImageAttachment {
    /// Image bytes as an attachment, checked as archond will check them.
    /// `name` is the file it was read from; it also names it in a refusal.
    pub fn encode(bytes: &[u8], name: Option<&str>) -> Result<Self, AttachmentError> {
        let label = name.unwrap_or("from the clipboard");
        let Some(media_type) = sniff_media_type(bytes) else {
            return Err(AttachmentError::UnsupportedType {
                label: label.to_owned(),
            });
        };
        let checked = check_image(bytes, media_type, name, label)?;
        assert_eq!(checked.size_bytes, bytes.len() as u64);
        let image = Self {
            media_type: checked.media_type,
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            name: checked.name,
        };
        debug_assert_eq!(image.size_bytes(), bytes.len() as u64);
        Ok(image)
    }

    /// Its size once decoded, read off the base64 length.
    pub fn size_bytes(&self) -> u64 {
        let symbols = self.data.trim_end_matches('=').len() as u64;
        symbols * 3 / 4
    }

    /// What the feed keeps of it, without decoding it.
    pub fn reference(&self) -> ImageRef {
        ImageRef {
            media_type: self.media_type.clone(),
            size_bytes: self.size_bytes(),
            name: self.name.as_deref().and_then(clean_name),
        }
    }
}

// The payload is megabytes of base64; a debug print names it instead.
impl fmt::Debug for ImageAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageAttachment")
            .field("media_type", &self.media_type)
            .field("data", &format_args!("<{} base64 chars>", self.data.len()))
            .field("name", &self.name)
            .finish()
    }
}

/// An image a message carried, as the feed records it: no bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    pub media_type: String,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ImageRef {
    /// The image as a person names it: `shot.png, 1.2 MB`.
    pub fn summary(&self) -> String {
        let name = self.name.as_deref().unwrap_or(&self.media_type);
        format!("{name}, {}", human_size(self.size_bytes))
    }

    /// The stand-in shown wherever the image cannot be: `[image: shot.png, 1.2 MB]`.
    pub fn label(&self) -> String {
        format!("[image: {}]", self.summary())
    }
}

/// `text`, then one line per image label: how a message with images reads.
pub fn with_image_labels(text: &str, images: &[ImageRef]) -> String {
    let text = text.trim();
    let labels = images.iter().map(ImageRef::label);
    let lines: Vec<String> = (!text.is_empty())
        .then(|| text.to_owned())
        .into_iter()
        .chain(labels)
        .collect();
    lines.join("\n")
}

/// Why an image was refused; archond answers with this text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachmentError {
    #[error("too many images: {count} attached, at most {MAX_IMAGES_PER_MESSAGE} per message")]
    TooMany { count: usize },
    #[error("image {label} is not valid base64: {reason}")]
    NotBase64 { label: String, reason: String },
    #[error("image {label} is empty")]
    Empty { label: String },
    #[error(
        "image {label} is too large: {size} bytes, at most {MAX_IMAGE_BYTES} bytes ({limit}) per image",
        limit = human_size(MAX_IMAGE_BYTES as u64)
    )]
    TooLarge { label: String, size: u64 },
    #[error(
        "images are too large together: {size} bytes, at most {MAX_MESSAGE_IMAGE_BYTES} bytes ({limit}) per message",
        limit = human_size(MAX_MESSAGE_IMAGE_BYTES as u64)
    )]
    TotalTooLarge { size: u64 },
    #[error("image {label} is not a supported type: its bytes are not PNG, JPEG, GIF or WebP")]
    UnsupportedType { label: String },
    #[error("image {label} is declared as {declared} but its bytes are {actual}")]
    TypeMismatch {
        label: String,
        declared: String,
        actual: &'static str,
    },
}

/// Check every image of one message (count, base64, type, each size and the
/// total) and name what the feed records of each. The first broken rule wins.
pub fn check_images(images: &[ImageAttachment]) -> Result<Vec<ImageRef>, AttachmentError> {
    if images.len() > MAX_IMAGES_PER_MESSAGE {
        return Err(AttachmentError::TooMany {
            count: images.len(),
        });
    }
    let mut checked = Vec::with_capacity(images.len());
    for (at, image) in images.iter().enumerate() {
        let label = image
            .name
            .as_deref()
            .and_then(clean_name)
            .unwrap_or_else(|| format!("#{}", at + 1));
        checked.push(check_encoded(image, &label)?);
    }
    let total: u64 = checked.iter().map(|image| image.size_bytes).sum();
    if total > MAX_MESSAGE_IMAGE_BYTES as u64 {
        return Err(AttachmentError::TotalTooLarge { size: total });
    }
    assert_eq!(checked.len(), images.len(), "one reference per image");
    Ok(checked)
}

/// One image as sent: refused on its encoded length before it is decoded.
fn check_encoded(image: &ImageAttachment, label: &str) -> Result<ImageRef, AttachmentError> {
    if image.size_bytes() > MAX_IMAGE_BYTES as u64 {
        return Err(AttachmentError::TooLarge {
            label: label.to_owned(),
            size: image.size_bytes(),
        });
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .map_err(|e| AttachmentError::NotBase64 {
            label: label.to_owned(),
            reason: e.to_string(),
        })?;
    let checked = check_image(&bytes, &image.media_type, image.name.as_deref(), label)?;
    debug_assert_eq!(checked.size_bytes, bytes.len() as u64);
    Ok(checked)
}

/// Check one decoded image against the type and size rules.
fn check_image(
    bytes: &[u8],
    declared: &str,
    name: Option<&str>,
    label: &str,
) -> Result<ImageRef, AttachmentError> {
    let label = || label.to_owned();
    if bytes.is_empty() {
        return Err(AttachmentError::Empty { label: label() });
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(AttachmentError::TooLarge {
            label: label(),
            size: bytes.len() as u64,
        });
    }
    let Some(actual) = sniff_media_type(bytes) else {
        return Err(AttachmentError::UnsupportedType { label: label() });
    };
    if declared != actual {
        return Err(AttachmentError::TypeMismatch {
            label: label(),
            declared: declared.to_owned(),
            actual,
        });
    }
    Ok(ImageRef {
        media_type: actual.to_owned(),
        size_bytes: bytes.len() as u64,
        name: name.and_then(clean_name),
    })
}

/// The image type these bytes are, read from their leading magic bytes.
pub fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// A file name safe to print in a prompt and a terminal: the last path
/// component, on one line, without control characters, cut to a fixed length.
pub fn clean_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && !crate::queue::is_bidi_control(*c))
        .take(MAX_ATTACHMENT_NAME_CHARS)
        .collect();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_owned())
}

/// A byte count as a person reads it: `812 B`, `34.1 KB`, `1.2 MB`.
pub fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if bytes < KB {
        return format!("{bytes} B");
    }
    if bytes < MB {
        return format!("{:.1} KB", bytes as f64 / KB as f64);
    }
    format!("{:.1} MB", bytes as f64 / MB as f64)
}

#[cfg(test)]
mod tests;
