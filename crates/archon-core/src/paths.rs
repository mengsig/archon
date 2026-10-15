//! Where Archon keeps its files. Environment overrides: `ARCHON_CONFIG_DIR`,
//! `ARCHON_DATA_DIR`, `ARCHON_SOCKET`.

use std::path::{Component, Path, PathBuf};

pub const CONFIG_DIR_VAR: &str = "ARCHON_CONFIG_DIR";
pub const DATA_DIR_VAR: &str = "ARCHON_DATA_DIR";
pub const SOCKET_VAR: &str = "ARCHON_SOCKET";

/// `config.toml` and user pipelines (`pipelines/<name>/`).
pub fn config_dir() -> PathBuf {
    std::env::var_os(CONFIG_DIR_VAR).map_or_else(default_config_dir, PathBuf::from)
}

/// SQLite store, worktrees, orchestrator workspace, logs, reports.
pub fn data_dir() -> PathBuf {
    std::env::var_os(DATA_DIR_VAR).map_or_else(default_data_dir, PathBuf::from)
}

fn default_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("archon")
}

fn default_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("archon")
}

pub const STORE_FILE_NAME: &str = "archon.db";

pub fn store_path() -> PathBuf {
    data_dir().join(STORE_FILE_NAME)
}

/// The lock archond holds in its data directory for as long as it runs, so
/// a second daemon never opens the same store (see `archon_queue::DataLock`).
pub const DATA_LOCK_FILE_NAME: &str = "data.lock";

pub fn data_lock_path_for(data_dir: &Path) -> PathBuf {
    data_dir.join(DATA_LOCK_FILE_NAME)
}

/// Where `cargo install` puts binaries: `$CARGO_HOME/bin`, else `~/.cargo/bin`.
pub fn cargo_bin_dir() -> Option<PathBuf> {
    let home = match std::env::var_os("CARGO_HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => dirs::home_dir()?.join(".cargo"),
    };
    // Compared with a canonical executable path; a missing one matches nothing.
    home.join("bin").canonicalize().ok()
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

/// The Archon daemon's Unix socket. Prefers the runtime dir so a reboot
/// clears it, exactly as Aether does for its own.
pub fn socket_path() -> PathBuf {
    std::env::var_os(SOCKET_VAR).map_or_else(|| default_socket_in(&data_dir()), PathBuf::from)
}

fn default_socket_in(data_dir: &Path) -> PathBuf {
    match dirs::runtime_dir() {
        Some(runtime) => runtime.join("archon").join("archond.sock"),
        None => data_dir.join("archond.sock"),
    }
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

/// One of Archon's three locations: where it resolves, and where it would
/// with no `ARCHON_*` variable set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub variable: &'static str,
    pub path: PathBuf,
    pub default: PathBuf,
}

impl Location {
    /// Compared where the filesystem leads them, symlinks and `..` resolved,
    /// so another spelling of the default is no move.
    pub fn moved(&self) -> bool {
        match (resolved(&self.path), resolved(&self.default)) {
            (Ok(path), Ok(default)) => path != default,
            // No working directory to resolve against: compare as written.
            _ => self.path != self.default,
        }
    }
}

/// `path` made absolute, its longest resolvable prefix canonicalized and the
/// rest, not created yet, appended with `..` taken lexically.
fn resolved(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let parts: Vec<Component<'_>> = absolute.components().collect();
    assert!(absolute.has_root(), "{} is absolute", absolute.display());
    for end in (1..=parts.len()).rev() {
        let prefix: PathBuf = parts[..end].iter().collect();
        // Missing, or not readable by this user: resolve what lies above it.
        let Ok(mut real) = prefix.canonicalize() else {
            continue;
        };
        for part in &parts[end..] {
            match part {
                Component::ParentDir => {
                    real.pop();
                }
                other => real.push(other),
            }
        }
        assert!(
            !real.components().any(|c| c == Component::ParentDir),
            "{} is resolved",
            real.display()
        );
        return Ok(real);
    }
    // Not even the root resolved: there is nothing to follow.
    Ok(absolute)
}

/// The config directory, the data directory and `socket` as this process
/// resolves them; an unset socket without a runtime dir moves with the data.
pub fn locations(socket: &Path) -> [Location; 3] {
    let default_data = default_data_dir();
    [
        Location {
            variable: CONFIG_DIR_VAR,
            path: config_dir(),
            default: default_config_dir(),
        },
        Location {
            variable: DATA_DIR_VAR,
            path: data_dir(),
            default: default_data.clone(),
        },
        Location {
            variable: SOCKET_VAR,
            path: socket.to_owned(),
            default: default_socket_in(&default_data),
        },
    ]
}

/// Some locations point away from their defaults and some do not, such as a private config and
/// socket over the default data dir; an archond started there would mix two setups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialOverride {
    pub moved: Vec<Location>,
    pub kept: Vec<Location>,
}

impl PartialOverride {
    /// `None` when the locations are all at their defaults or all moved.
    pub fn among(locations: [Location; 3]) -> Option<Self> {
        let (moved, kept): (Vec<_>, Vec<_>) = locations.into_iter().partition(Location::moved);
        assert_eq!(moved.len() + kept.len(), 3, "every location is sorted");
        if moved.is_empty() || kept.is_empty() {
            return None;
        }
        Some(Self { moved, kept })
    }
}

impl std::fmt::Display for PartialOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        assert!(!self.moved.is_empty() && !self.kept.is_empty(), "a mix");
        let point = if self.moved.len() == 1 {
            "points"
        } else {
            "point"
        };
        let are = if self.kept.len() == 1 { "is" } else { "are" };
        write!(
            f,
            "{} {point} away from the default, but {} {are} still the default, so an archond \
             started here would mix two setups. Set {CONFIG_DIR_VAR}, {DATA_DIR_VAR} and \
             {SOCKET_VAR} together, or none of them",
            listed(&self.moved),
            listed(&self.kept)
        )
    }
}

/// `A (/a)`, `A (/a) and B (/b)`.
fn listed(locations: &[Location]) -> String {
    let named: Vec<String> = locations
        .iter()
        .map(|l| format!("{} ({})", l.variable, l.path.display()))
        .collect();
    match named.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// Whether starting archond on `socket` would mix moved and default
/// locations; such a start is refused unless the operator runs it directly.
pub fn partial_override(socket: &Path) -> Option<PartialOverride> {
    PartialOverride::among(locations(socket))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(
        variable: &'static str,
        path: impl Into<PathBuf>,
        default: impl Into<PathBuf>,
    ) -> Location {
        Location {
            variable,
            path: path.into(),
            default: default.into(),
        }
    }

    fn mix(config: &str, data: &str, socket: &str) -> Option<PartialOverride> {
        PartialOverride::among([
            at(CONFIG_DIR_VAR, config, "/home/u/.config/archon"),
            at(DATA_DIR_VAR, data, "/home/u/.local/share/archon"),
            at(SOCKET_VAR, socket, "/run/user/1/archon/archond.sock"),
        ])
    }

    #[test]
    fn all_default_or_all_moved_is_no_mix() {
        let defaults = mix(
            "/home/u/.config/archon",
            "/home/u/.local/share/archon/",
            "/run/user/1/archon/./archond.sock",
        );
        assert_eq!(defaults, None, "the same places, however written");
        assert_eq!(mix("/p/config", "/p/data", "/p/archond.sock"), None);
    }

    #[test]
    fn a_private_config_and_socket_with_the_default_data_is_a_mix() {
        let found = mix(
            "/p/config",
            "/home/u/.local/share/archon",
            "/p/archond.sock",
        )
        .expect("a mix");
        let moved: Vec<_> = found.moved.iter().map(|l| l.variable).collect();
        assert_eq!(moved, [CONFIG_DIR_VAR, SOCKET_VAR]);
        assert_eq!(found.kept[0].variable, DATA_DIR_VAR);
        let text = found.to_string();
        assert!(
            text.starts_with(
                "ARCHON_CONFIG_DIR (/p/config) and ARCHON_SOCKET (/p/archond.sock) point away"
            ),
            "{text}"
        );
        assert!(
            text.contains("but ARCHON_DATA_DIR (/home/u/.local/share/archon) is still"),
            "{text}"
        );
    }

    #[test]
    fn any_one_location_moved_alone_is_a_mix() {
        let config = mix(
            "/p/c",
            "/home/u/.local/share/archon",
            "/run/user/1/archon/archond.sock",
        );
        let data = mix(
            "/home/u/.config/archon",
            "/p/d",
            "/run/user/1/archon/archond.sock",
        );
        let socket = mix(
            "/home/u/.config/archon",
            "/home/u/.local/share/archon",
            "/p/s",
        );
        for found in [config, data, socket] {
            let found = found.expect("a mix");
            assert_eq!((found.moved.len(), found.kept.len()), (1, 2));
            assert!(found.to_string().contains(" points away"), "{found}");
            assert!(
                found.to_string().contains(" are still the default"),
                "{found}"
            );
        }
    }

    /// A private config and socket over `data`, which leads to `<root>/share/archon`.
    fn over_data(root: &Path, data: PathBuf) -> Option<PartialOverride> {
        PartialOverride::among([
            at(
                CONFIG_DIR_VAR,
                root.join("p/config"),
                root.join("config/archon"),
            ),
            at(DATA_DIR_VAR, data, root.join("share/archon")),
            at(
                SOCKET_VAR,
                root.join("p/s.sock"),
                root.join("run/archon/archond.sock"),
            ),
        ])
    }

    #[test]
    fn another_way_to_the_default_data_dir_is_still_the_default() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let default = root.join("share/archon");
        std::fs::create_dir_all(&default).unwrap();
        std::os::unix::fs::symlink(&default, root.join("data-link")).unwrap();
        let spellings = [
            root.join("data-link"),
            default.join("../archon"),
            // `..` leaves the link's target, not the link: `share/archon` again.
            root.join("data-link/../archon"),
        ];
        for data in spellings {
            let found = over_data(root, data.clone())
                .unwrap_or_else(|| panic!("{} is the default data dir", data.display()));
            let kept: Vec<_> = found.kept.iter().map(|l| l.variable).collect();
            assert_eq!(kept, [DATA_DIR_VAR], "{}", data.display());
            assert!(
                found.to_string().contains("is still the default"),
                "{found}"
            );
        }
        assert_eq!(over_data(root, root.join("p/data")), None, "all moved");
    }

    #[test]
    fn a_symlink_to_the_default_config_moves_nothing() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let default = root.join("config/archon");
        std::fs::create_dir_all(&default).unwrap();
        let link = root.join("config-link");
        std::os::unix::fs::symlink(&default, &link).unwrap();
        let sock = root.join("run/archon/archond.sock");
        let found = PartialOverride::among([
            at(CONFIG_DIR_VAR, &link, &default),
            at(
                DATA_DIR_VAR,
                root.join("share/archon"),
                root.join("share/archon"),
            ),
            at(SOCKET_VAR, &sock, &sock),
        ]);
        assert_eq!(found, None, "the default config, reached through a symlink");
        assert!(at(CONFIG_DIR_VAR, root.join("config"), &default).moved());
    }

    #[test]
    fn a_path_not_created_yet_resolves_through_its_nearest_existing_ancestor() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        std::fs::create_dir(root.join("run")).unwrap();
        std::os::unix::fs::symlink(root.join("run"), root.join("run-link")).unwrap();
        let default = root.join("run/archon/archond.sock");
        let socket = |path: PathBuf| at(SOCKET_VAR, path, &default);
        assert!(!socket(root.join("run-link/archon/archond.sock")).moved());
        assert!(!socket(root.join("run/archon/x/../archond.sock")).moved());
        assert!(socket(root.join("run-link/other/archond.sock")).moved());
        assert!(
            !default.parent().unwrap().exists(),
            "none of it was created"
        );
    }

    #[test]
    fn the_default_socket_follows_the_data_dir_without_a_runtime_dir() {
        let data = Path::new("/p/data");
        let socket = default_socket_in(data);
        match dirs::runtime_dir() {
            Some(runtime) => assert!(socket.starts_with(runtime), "{}", socket.display()),
            None => assert_eq!(socket, data.join("archond.sock")),
        }
        assert_eq!(
            data_lock_path_for(data),
            data.join("data.lock"),
            "the lock sits beside the store"
        );
        assert_ne!(
            data_lock_path_for(data),
            default_socket_in(data).with_extension("lock"),
            "it never is the socket's own lock, even beside it"
        );
    }
}
