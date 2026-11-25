//! How a reference picture was taken, so a built picture is taken the same
//! way and the two compare like with like.

use serde::{Deserialize, Serialize};

use super::Viewport;

/// How a picture was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMethod {
    /// The browser's own screenshot of the page (Chromium's
    /// `Page.captureScreenshot`): no browser chrome, toolbar or design overlay.
    PageScreenshot,
}

/// A rectangle of the page in whole CSS pixels, from its top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CaptureArea {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The conditions a reference picture was taken in. Its device scale factor is
/// `pixels` over `area`; a built picture taken the same way has its `pixels`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImageCapture {
    pub method: CaptureMethod,
    /// The CSS viewport the page was laid out at.
    pub viewport: Viewport,
    /// What the picture shows: the whole viewport, or a region of the page.
    pub area: CaptureArea,
    /// The picture's size in device pixels.
    pub pixels: Viewport,
}

/// Device pixels a built picture may differ by: scales below 1 round.
const PIXEL_TOLERANCE: u32 = 1;

impl ImageCapture {
    /// Whether every size is positive.
    pub fn is_valid(&self) -> bool {
        [
            self.viewport.width,
            self.viewport.height,
            self.area.width,
            self.area.height,
            self.pixels.width,
            self.pixels.height,
        ]
        .iter()
        .all(|&n| n > 0)
    }

    /// Device pixels per CSS pixel.
    pub fn scale(&self) -> f64 {
        assert!(self.area.width > 0, "a capture shows a nonempty area");
        f64::from(self.pixels.width) / f64::from(self.area.width)
    }

    /// Whether the picture shows the whole viewport rather than a region.
    pub fn whole_viewport(&self) -> bool {
        self.area
            == (CaptureArea {
                x: 0,
                y: 0,
                width: self.viewport.width,
                height: self.viewport.height,
            })
    }

    /// Whether a built picture of `width` × `height` device pixels can have
    /// been taken in these conditions.
    pub fn fits(&self, width: u32, height: u32) -> bool {
        self.pixels.width.abs_diff(width) <= PIXEL_TOLERANCE
            && self.pixels.height.abs_diff(height) <= PIXEL_TOLERANCE
    }

    /// The conditions in words, for a builder to reproduce and a judge to read.
    pub fn describe(&self) -> String {
        assert!(self.is_valid(), "only a valid capture is described");
        let area = if self.whole_viewport() {
            "the whole viewport, scrolled to the top".to_owned()
        } else {
            format!(
                "the page region x={} y={} {}x{} CSS px",
                self.area.x, self.area.y, self.area.width, self.area.height
            )
        };
        let text = format!(
            "a browser page screenshot (no browser chrome, toolbar or design overlay) of {area}, \
             with the page laid out at a {}x{} CSS px viewport and a device scale factor of {}, \
             so the image is {}x{} pixels",
            self.viewport.width,
            self.viewport.height,
            self.scale(),
            self.pixels.width,
            self.pixels.height
        );
        debug_assert!(text.contains(&format!(
            "{}x{} pixels",
            self.pixels.width, self.pixels.height
        )));
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s4_reference() -> ImageCapture {
        ImageCapture {
            method: CaptureMethod::PageScreenshot,
            viewport: Viewport {
                width: 1440,
                height: 900,
            },
            area: CaptureArea {
                x: 0,
                y: 0,
                width: 1440,
                height: 900,
            },
            pixels: Viewport {
                width: 2880,
                height: 1800,
            },
        }
    }

    /// S4: the reference was 2880x1800 at a 1440x900 viewport. A build taken
    /// at 720x450 CSS px and scale 2 is 1440x900 pixels: it does not fit.
    #[test]
    fn a_picture_fits_only_the_pixels_its_reference_was_taken_at() {
        let capture = s4_reference();
        assert!(capture.is_valid());
        assert!(capture.whole_viewport());
        assert_eq!(capture.scale(), 2.0);
        assert!(capture.fits(2880, 1800));
        assert!(capture.fits(2879, 1801));
        assert!(!capture.fits(1440, 900));
        let words = capture.describe();
        assert!(words.contains("1440x900 CSS px viewport"), "{words}");
        assert!(words.contains("device scale factor of 2"), "{words}");
        assert!(words.contains("2880x1800 pixels"), "{words}");
    }

    #[test]
    fn a_region_is_described_by_its_rectangle_and_an_empty_size_is_invalid() {
        let mut capture = s4_reference();
        capture.area = CaptureArea {
            x: 344,
            y: 24,
            width: 752,
            height: 120,
        };
        capture.pixels = Viewport {
            width: 1504,
            height: 240,
        };
        assert!(!capture.whole_viewport());
        assert!(capture.describe().contains("x=344 y=24 752x120 CSS px"));
        capture.pixels.height = 0;
        assert!(!capture.is_valid());
    }
}
