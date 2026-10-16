//! One error type for the whole of Archon. Variants name the boundary that
//! failed so a message to the operator can say where to look.

#[derive(Debug, thiserror::Error)]
pub enum ArchonError {
    #[error("configuration: {0}")]
    Config(String),
    #[error("invalid argument: {0}")]
    Invalid(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("state transition refused: {0}")]
    InvalidTransition(String),
    #[error("store: {0}")]
    Store(String),
    /// The store's disk is full (SQLite `SQLITE_FULL`): a store failure
    /// that names its cause, so the daemon can say so instead of crashing.
    #[error("store: disk full: {0}")]
    DiskFull(String),
    #[error("aether: {0}")]
    Aether(String),
    #[error("pipeline: {0}")]
    Pipeline(String),
    /// A model or effort outside the configured model policy.
    #[error("model policy: {0}")]
    Policy(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// The caller may not make this request (an operator-only request from
    /// the MCP proxy).
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// archond is still recovering its runs after a start and refuses
    /// changes until it is done; the same request succeeds once it is.
    #[error("recovering: {0}")]
    Recovering(String),
    /// The cause is part of the text and deliberately not `source()`: a
    /// chain printer (`{:#}`) would otherwise print it twice.
    #[error("io: {context}: {cause}")]
    Io {
        context: String,
        cause: std::io::Error,
    },
    #[error("protocol: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, ArchonError>;

impl ArchonError {
    /// Wrap an I/O error with the path or operation that produced it.
    pub fn io(context: impl Into<String>, cause: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            cause,
        }
    }

    /// A failure of Archon's own store, which the daemon never continues
    /// past (ARCHITECTURE §16); a full disk is one.
    pub fn is_store(&self) -> bool {
        matches!(self, Self::Store(_) | Self::DiskFull(_))
    }

    /// The same error, of the same kind, with `detail` appended to its
    /// message: what the caller had done already when it failed.
    pub fn with_detail(self, detail: &str) -> Self {
        assert!(!detail.trim().is_empty(), "a detail says something");
        let add = |message: String| format!("{message}; {detail}");
        match self {
            Self::Config(m) => Self::Config(add(m)),
            Self::Invalid(m) => Self::Invalid(add(m)),
            Self::NotFound(m) => Self::NotFound(add(m)),
            Self::InvalidTransition(m) => Self::InvalidTransition(add(m)),
            Self::Store(m) => Self::Store(add(m)),
            Self::DiskFull(m) => Self::DiskFull(add(m)),
            Self::Aether(m) => Self::Aether(add(m)),
            Self::Pipeline(m) => Self::Pipeline(add(m)),
            Self::Policy(m) => Self::Policy(add(m)),
            Self::Unavailable(m) => Self::Unavailable(add(m)),
            Self::Forbidden(m) => Self::Forbidden(add(m)),
            Self::Recovering(m) => Self::Recovering(add(m)),
            Self::Io { context, cause } => Self::Io {
                context: add(context),
                cause,
            },
            Self::Protocol(m) => Self::Protocol(add(m)),
        }
    }
}

impl From<serde_json::Error> for ArchonError {
    fn from(e: serde_json::Error) -> Self {
        Self::Protocol(format!("json: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_io_error_names_its_cause_exactly_once() {
        let cause = std::io::Error::new(std::io::ErrorKind::InvalidInput, "path too long");
        let error = ArchonError::io("connecting to /x.sock", cause);
        assert_eq!(
            error.to_string(),
            "io: connecting to /x.sock: path too long"
        );
        assert!(
            std::error::Error::source(&error).is_none(),
            "a chain printer would repeat the cause"
        );
    }

    #[test]
    fn a_detail_keeps_the_errors_kind_and_its_message() {
        let full = ArchonError::DiskFull("update item".into()).with_detail("kept at /x");
        assert!(full.is_store(), "a store failure stays one");
        assert_eq!(
            full.to_string(),
            "store: disk full: update item; kept at /x"
        );
        let cause = std::io::Error::other("gone");
        let io = ArchonError::io("reading /y", cause).with_detail("kept at /x");
        assert_eq!(io.to_string(), "io: reading /y; kept at /x: gone");
    }

    #[test]
    fn a_full_disk_is_a_store_failure_that_says_so() {
        let full = ArchonError::DiskFull("insert feed entry: database or disk is full".into());
        assert!(full.is_store());
        assert!(ArchonError::Store("decode item".into()).is_store());
        assert!(!ArchonError::Aether("internal: disk is full".into()).is_store());
        assert_eq!(
            full.to_string(),
            "store: disk full: insert feed entry: database or disk is full"
        );
    }
}
