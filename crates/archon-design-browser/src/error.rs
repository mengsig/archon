//! Errors of the browser driver.

/// Why a browser operation failed.
#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    /// No Chromium binary was found.
    #[error("no browser found: {hint}")]
    NotFound {
        /// How to install one.
        hint: String,
    },
    /// Chromium's sandbox cannot start on this host.
    #[error("browser sandbox unavailable: {hint}")]
    SandboxUnavailable {
        /// The fix.
        hint: String,
    },
    /// The process could not be started.
    #[error("browser launch failed: {0}")]
    Launch(String),
    /// The browser sent something the driver cannot use, or CDP answered with an error.
    #[error("browser protocol error: {0}")]
    Protocol(String),
    /// The browser is gone.
    #[error("browser closed")]
    Closed,
    /// A document policy failure stopped the browser.
    #[error("preview containment stopped: {0}")]
    Containment(String),
    /// The page could not be verified for this read.
    #[error("preview document changed or is not ready")]
    DisclosureUnavailable,
    /// A file input's `accept` is longer than the preview checks; it is refused, never cut.
    #[error("the file input accepts too many types to check ({bytes} bytes; at most {max})")]
    AcceptTooLong {
        /// Its length without blanks and control characters.
        bytes: usize,
        /// The longest checked.
        max: usize,
    },
    /// The page's dialog is open, and Chrome holds the call until it is answered.
    #[error("the app has a dialog open; answer it first")]
    DialogOpen,
    /// A call took too long.
    #[error("browser call timed out: {0}")]
    Timeout(String),
    /// A navigation failed.
    #[error("navigation to {url} failed: {error}")]
    Navigation {
        /// The target URL.
        url: String,
        /// The browser's error text.
        error: String,
    },
}
