//! Where Archon keeps its files. Environment overrides: `ARCHON_CONFIG_DIR`,
//! `ARCHON_DATA_DIR`, `ARCHON_SOCKET`.

use std::path::{Path, PathBuf};

/// `config.toml` and user pipelines (`pipelines/<name>/`).
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ARCHON_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("archon")
}

/// SQLite store, worktrees, orchestrator workspace, logs, reports.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ARCHON_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("archon")
}

pub const STORE_FILE_NAME: &str = "archon.db";

pub fn store_path() -> PathBuf {
    data_dir().join(STORE_FILE_NAME)
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

/// The Archon daemon's Unix socket. Prefers the runtime dir so a reboot
/// clears it, exactly as Aether does for its own.
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ARCHON_SOCKET") {
        return PathBuf::from(path);
    }
    if let Some(runtime) = dirs::runtime_dir() {
        return runtime.join("archon").join("archond.sock");
    }
    data_dir().join("archond.sock")
}

pub fn pid_path_for(socket: &Path) -> PathBuf {
    socket.with_extension("pid")
}

/// Where `archon daemon run --detach` writes the detached daemon's startup
/// output, so a client that autostarted it can show why it never listened.
pub fn startup_log_for(socket: &Path) -> PathBuf {
    socket.with_extension("log")
}

/// Starts the line each detached start appends to the startup log; what
/// follows the last one is the current daemon's output.
pub const STARTUP_LOG_MARK: &str = "--- archond starting";

/// Where a repository keeps its Archon overrides: `.archon/config.toml` and
/// `.archon/pipelines/<name>/`.
pub fn repo_archon_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".archon")
}
