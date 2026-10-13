//! Archon's own Aether instance (ARCHITECTURE §4.7): in dedicated mode
//! Archon runs an `aetherd` whose config, data and socket live under
//! Archon's directories, so plain Aether never loads Archon's MCP server.

pub mod lifecycle;
pub mod seed;

use std::ffi::OsStr;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context as _;
use archon_core::config::{AetherConfig, AetherMode};

/// The `aether` CLI, looked up on PATH.
pub const AETHER_PROGRAM: &str = "aether";
/// The instance's socket file, beside archond's own.
const SOCKET_NAME: &str = "aetherd.sock";
/// Serializes `aether daemon` starts across concurrent archon processes.
const START_LOCK_NAME: &str = "aetherd-start.lock";
/// Present while a running aetherd may hold a registration Archon replaced.
const RELOAD_MARKER_NAME: &str = "aetherd-reload-owed";

/// Where the dedicated instance lives, and the Archon paths its `archon mcp`
/// must reach. Every path is absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub archon_config_dir: PathBuf,
    pub archon_data_dir: PathBuf,
    pub archon_socket: PathBuf,
    /// `AETHER_CONFIG_DIR`: `<archon config>/aether`.
    pub config_dir: PathBuf,
    /// `AETHER_DATA_DIR`: `<archon data>/aether`.
    pub data_dir: PathBuf,
    /// `AETHER_SOCKET`: `aetherd.sock` beside archond's socket.
    pub socket: PathBuf,
}

impl Instance {
    /// The instance for this process's Archon directories.
    pub fn resolve() -> anyhow::Result<Self> {
        let absolute = |path: PathBuf| {
            std::path::absolute(&path).with_context(|| format!("resolving {}", path.display()))
        };
        Ok(Self::from_archon_dirs(
            absolute(archon_core::paths::config_dir())?,
            absolute(archon_core::paths::data_dir())?,
            absolute(archon_core::paths::socket_path())?,
        ))
    }

    /// Derives every instance path from Archon's own, and nothing else.
    pub fn from_archon_dirs(
        config_dir: PathBuf,
        data_dir: PathBuf,
        archon_socket: PathBuf,
    ) -> Self {
        assert!(
            config_dir.is_absolute() && data_dir.is_absolute() && archon_socket.is_absolute(),
            "aetherd runs in another directory, so every path must be absolute"
        );
        let socket = archon_socket.with_file_name(SOCKET_NAME);
        assert_ne!(socket, archon_socket, "archond's socket is never aetherd's");
        Self {
            config_dir: config_dir.join("aether"),
            data_dir: data_dir.join("aether"),
            socket,
            archon_config_dir: config_dir,
            archon_data_dir: data_dir,
            archon_socket,
        }
    }

    pub fn config_path(&self) -> PathBuf {
        self.config_dir.join(archon_core::config::CONFIG_FILE_NAME)
    }

    pub fn start_lock_path(&self) -> PathBuf {
        self.archon_data_dir.join(START_LOCK_NAME)
    }

    pub fn reload_marker_path(&self) -> PathBuf {
        self.archon_data_dir.join(RELOAD_MARKER_NAME)
    }

    /// What every `aether` process of this instance gets: Aether's three
    /// locations, and Archon's so the `archon mcp` it spawns finds archond.
    pub fn env(&self) -> [(&'static str, &Path); 6] {
        [
            ("AETHER_CONFIG_DIR", &self.config_dir),
            ("AETHER_DATA_DIR", &self.data_dir),
            ("AETHER_SOCKET", &self.socket),
            ("ARCHON_CONFIG_DIR", &self.archon_config_dir),
            ("ARCHON_DATA_DIR", &self.archon_data_dir),
            ("ARCHON_SOCKET", &self.archon_socket),
        ]
    }

    /// `aether <args>` against this instance.
    pub fn aether_command<I, S>(&self, aether: &Path, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(aether);
        command.args(args);
        for (name, value) in self.env() {
            command.env(name, value);
        }
        command
    }
}

/// The Aether endpoint archond and `archon doctor` use: the instance's
/// socket in dedicated mode, the configured one in shared mode.
pub fn effective_aether(config: &AetherConfig, instance: &Instance) -> AetherConfig {
    match config.mode {
        AetherMode::Shared => config.clone(),
        AetherMode::Dedicated => {
            assert!(
                config.socket.is_none() && config.address.is_none(),
                "config validation refuses an endpoint in dedicated mode"
            );
            AetherConfig {
                socket: Some(instance.socket.clone()),
                ..config.clone()
            }
        }
    }
}

/// Create `dir` and its missing parents, private to the operator (0700)
/// where this creates them: Aether refuses credentials in a group- or
/// world-writable directory. An existing directory is left as it is.
pub fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    assert!(
        dir.is_absolute(),
        "private directories are created by absolute path"
    );
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    assert!(dir.is_dir(), "a created directory exists");
    Ok(())
}

/// The operator's own Aether config directory, as Aether resolves it.
pub fn operator_config_dir() -> anyhow::Result<PathBuf> {
    let dir = crate::setup::aether_config_path()
        .parent()
        .map(Path::to_path_buf)
        .context("Aether's config path has no directory")?;
    std::path::absolute(&dir).with_context(|| format!("resolving {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance() -> Instance {
        Instance::from_archon_dirs(
            "/home/op/.config/archon".into(),
            "/home/op/.local/share/archon".into(),
            "/run/user/1000/archon/archond.sock".into(),
        )
    }

    #[test]
    fn every_path_is_inside_archons_own_directories() {
        let instance = instance();
        assert!(instance.config_dir.starts_with("/home/op/.config/archon"));
        assert!(
            instance
                .data_dir
                .starts_with("/home/op/.local/share/archon")
        );
        assert_eq!(
            instance.socket,
            Path::new("/run/user/1000/archon/aetherd.sock")
        );
        assert!(
            instance
                .start_lock_path()
                .starts_with(&instance.archon_data_dir)
        );
        let archon_dirs = [
            Path::new("/home/op/.config/archon"),
            Path::new("/home/op/.local/share/archon"),
            Path::new("/run/user/1000/archon"),
        ];
        for (name, value) in instance.env() {
            assert!(
                archon_dirs.iter().any(|dir| value.starts_with(dir)),
                "{name}={} leaves Archon's directories",
                value.display()
            );
            assert!(!value.starts_with("/tmp"), "{name} is in /tmp");
            assert!(
                !value.starts_with("/home/op/.config/aether")
                    && !value.starts_with("/home/op/.local/share/aether")
                    && !value.starts_with("/run/user/1000/aether"),
                "{name} is in the operator's Aether directories"
            );
        }
    }

    #[test]
    fn the_command_carries_the_whole_instance_environment() {
        let instance = instance();
        let command = instance.aether_command(Path::new("aether"), ["agents"]);
        let envs: Vec<_> = command.get_envs().collect();
        assert_eq!(envs.len(), 6);
        let socket = envs.iter().find(|(k, _)| *k == "AETHER_SOCKET").unwrap();
        assert_eq!(socket.1, Some(instance.socket.as_os_str()));
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["agents"]);
    }

    #[test]
    fn only_dedicated_mode_rewrites_the_endpoint() {
        let instance = instance();
        let dedicated = effective_aether(&AetherConfig::default(), &instance);
        assert_eq!(dedicated.socket.as_deref(), Some(instance.socket.as_path()));
        let shared = AetherConfig {
            mode: AetherMode::Shared,
            ..AetherConfig::default()
        };
        assert_eq!(effective_aether(&shared, &instance), shared);
    }
}
