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

/// The conditions a reference picture was taken in: the page's emulation, the
/// area shown, and the screenshot's own output scale, which is not the page's.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImageCapture {
    pub method: CaptureMethod,
    /// The CSS viewport the page was laid out at.
    pub viewport: Viewport,
    /// The page's emulated device pixel ratio, which CSS resolution queries,
    /// `srcset` and `window.devicePixelRatio` see.
    pub device_scale_factor: f64,
    /// Whether the page was emulated as a mobile device with touch input: the
    /// preview sets both from this one bit, so a replay must too.
    pub mobile: bool,
    /// What the picture shows: the whole viewport, or a region of the page.
    pub area: CaptureArea,
    /// The screenshot's `clip.scale`: device pixels per CSS px in the image.
    pub screenshot_scale: f64,
    /// The picture's size in device pixels.
    pub pixels: Viewport,
}

/// JSON has no NaN, and `is_valid` refuses one, so equality is total.
impl Eq for ImageCapture {}

/// Device pixels a built picture may differ by: scales below 1 round.
const PIXEL_TOLERANCE: u32 = 1;

impl ImageCapture {
    /// Whether every size and scale is positive and finite.
    pub fn is_valid(&self) -> bool {
        let sizes = [
            self.viewport.width,
            self.viewport.height,
            self.area.width,
            self.area.height,
            self.pixels.width,
            self.pixels.height,
        ];
        let scales = [self.device_scale_factor, self.screenshot_scale];
        sizes.iter().all(|&n| n > 0) && scales.iter().all(|s| s.is_finite() && *s > 0.0)
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

    /// The conditions as a recipe, for a builder to replay and a judge to read.
    pub fn describe(&self) -> String {
        assert!(self.is_valid(), "only a valid capture is described");
        let (vw, vh, a) = (self.viewport.width, self.viewport.height, self.area);
        let text = format!(
            "a Chromium page screenshot with no browser chrome, toolbar or design overlay: \
             emulate the page at a {vw}x{vh} CSS px viewport with deviceScaleFactor {dsf} and \
             mobile {mobile} (Emulation.setDeviceMetricsOverride), with touch {mobile} \
             (Emulation.setTouchEmulationEnabled; in Playwright, viewport {vw}x{vh}, \
             deviceScaleFactor {dsf}, isMobile {mobile}, hasTouch {mobile}), {top}then call \
             Page.captureScreenshot with clip {{x: {}, y: {}, width: {}, height: {}, scale: {s}}} \
             (in Playwright, through a CDP session), never a larger deviceScaleFactor in place \
             of the clip scale, so the image is {}x{} pixels",
            a.x,
            a.y,
            a.width,
            a.height,
            self.pixels.width,
            self.pixels.height,
            dsf = self.device_scale_factor,
            mobile = self.mobile,
            top = if self.whole_viewport() {
                "scroll to the top, "
            } else {
                ""
            },
            s = self.screenshot_scale,
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

    /// The S4 reference: the preview's page at DPR 1, shot at clip scale 2.
    fn s4_reference() -> ImageCapture {
        ImageCapture {
            method: CaptureMethod::PageScreenshot,
            viewport: Viewport {
                width: 1440,
                height: 900,
            },
            device_scale_factor: 1.0,
            mobile: false,
            area: CaptureArea {
                x: 0,
                y: 0,
                width: 1440,
                height: 900,
            },
            screenshot_scale: 2.0,
            pixels: Viewport {
                width: 2880,
                height: 1800,
            },
        }
    }

    /// S4: a build taken at 720x450 CSS px and scale 2 is 1440x900 pixels, so
    /// it does not fit; the recipe keeps the page at DPR 1 and the clip at 2.
    #[test]
    fn a_picture_fits_only_the_pixels_its_reference_was_taken_at() {
        let capture = s4_reference();
        assert!(capture.is_valid());
        assert!(capture.whole_viewport());
        assert!(capture.fits(2880, 1800));
        assert!(capture.fits(2879, 1801));
        assert!(!capture.fits(1440, 900));
        let words = capture.describe();
        for part in [
            "1440x900 CSS px viewport with deviceScaleFactor 1 and mobile false",
            "isMobile false, hasTouch false",
            "with touch false (Emulation.setTouchEmulationEnabled",
            "scroll to the top",
            "clip {x: 0, y: 0, width: 1440, height: 900, scale: 2}",
            "2880x1800 pixels",
        ] {
            assert!(words.contains(part), "{part}: {words}");
        }
    }

    /// The reviewer's probe: DPR 2 at clip scale 1 makes the same pixels but
    /// another layout, so it is another capture.
    #[test]
    fn the_page_dpr_and_the_clip_scale_are_different_conditions() {
        let reference = s4_reference();
        let replayed = ImageCapture {
            device_scale_factor: 2.0,
            screenshot_scale: 1.0,
            ..reference
        };
        assert!(replayed.fits(2880, 1800));
        assert_ne!(reference, replayed);
        let json = serde_json::to_value(reference).unwrap();
        assert_eq!(json["device_scale_factor"], 1.0);
        assert_eq!(json["screenshot_scale"], 2.0);
        let back: ImageCapture = serde_json::from_value(json).unwrap();
        assert_eq!(back, reference);
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
        let words = capture.describe();
        assert!(words.contains("clip {x: 344, y: 24, width: 752, height: 120, scale: 2}"));
        assert!(!words.contains("scroll to the top"), "{words}");
        capture.screenshot_scale = f64::NAN;
        assert!(!capture.is_valid());
        capture.screenshot_scale = 2.0;
        capture.pixels.height = 0;
        assert!(!capture.is_valid());
    }
}
