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
