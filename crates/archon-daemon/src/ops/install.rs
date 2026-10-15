//! How an upgrade installs a program: the way its binary was installed.
//! A binary in cargo's bin directory is rebuilt with `cargo install --git`,
//! as `make install` built it from a checkout; any other is the installer's,
//! replaced by its own `<program> update`.

use std::fmt;
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::ops::UpgradeTarget;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Program {
    Archon,
    Aether,
}

impl Program {
    pub fn name(self) -> &'static str {
        match self {
            Self::Archon => "archon",
            Self::Aether => "aether",
        }
    }

    /// The git repository `cargo install --git` builds the program from.
    fn source(self) -> &'static str {
        match self {
            Self::Archon => "https://github.com/mengsig/archon",
            Self::Aether => "https://github.com/mengsig/aether",
        }
    }
}

impl fmt::Display for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One install command: `command` with `args`, no shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallStep {
    pub program: Program,
    pub command: PathBuf,
    pub args: Vec<String>,
}

impl InstallStep {
    pub fn describe(&self) -> String {
        format!("`{} {}`", self.command.display(), self.args.join(" "))
    }
}

/// Whether `binary` sits in cargo's bin directory `cargo_bin`.
pub fn by_cargo(binary: &Path, cargo_bin: Option<&Path>) -> bool {
    cargo_bin.is_some_and(|dir| binary.parent() == Some(dir))
}

/// The command that installs `target` over `binary`. A cargo build has no
/// "latest" of its own, so the caller names that release first.
pub fn plan(
    program: Program,
    target: &UpgradeTarget,
    binary: &Path,
    cargo_bin: Option<&Path>,
) -> Result<InstallStep, ArchonError> {
    assert!(binary.is_absolute(), "{} is located", binary.display());
    let args: Vec<String> = match (by_cargo(binary, cargo_bin), target) {
        (true, UpgradeTarget::Latest) => {
            return Err(ArchonError::Invalid(format!(
                "{program} at {} is a cargo build; name a release or main",
                binary.display()
            )));
        }
        (true, UpgradeTarget::Main) => cargo_args(program, ["--branch", "main"]),
        (true, UpgradeTarget::Version(version)) => {
            cargo_args(program, ["--tag", &format!("v{version}")])
        }
        (false, UpgradeTarget::Main) => {
            return Err(ArchonError::Invalid(format!(
                "{program} at {} came from a release installer, and main is built by cargo; \
                 name a release, or `cargo install` {program} first",
                binary.display()
            )));
        }
        (false, UpgradeTarget::Latest) => vec!["update".into()],
        (false, UpgradeTarget::Version(version)) => {
            vec!["update".into(), "--version".into(), format!("v{version}")]
        }
    };
    let command = match by_cargo(binary, cargo_bin) {
        true => PathBuf::from("cargo"),
        false => binary.to_path_buf(),
    };
    Ok(InstallStep {
        program,
        command,
        args,
    })
}

fn cargo_args(program: Program, revision: [&str; 2]) -> Vec<String> {
    let [flag, value] = revision;
    [
        "install",
        "--locked",
        "--git",
        program.source(),
        flag,
        value,
    ]
    .into_iter()
    .chain([program.name()])
    .map(str::to_owned)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARGO_BIN: &str = "/home/op/.cargo/bin";

    fn plan_for(binary: &str, target: UpgradeTarget) -> Result<InstallStep, ArchonError> {
        plan(
            Program::Archon,
            &target,
            Path::new(binary),
            Some(Path::new(CARGO_BIN)),
        )
    }

    #[test]
    fn a_cargo_build_is_rebuilt_from_its_git_repository() {
        let main = plan_for("/home/op/.cargo/bin/archon", UpgradeTarget::Main).unwrap();
        assert_eq!(main.command, Path::new("cargo"));
        assert_eq!(
            main.args.join(" "),
            "install --locked --git https://github.com/mengsig/archon --branch main archon"
        );
        let release = plan_for(
            "/home/op/.cargo/bin/archon",
            UpgradeTarget::Version("1.2.3".into()),
        )
        .unwrap();
        assert!(release.args.join(" ").ends_with("--tag v1.2.3 archon"));
        let latest = plan_for("/home/op/.cargo/bin/archon", UpgradeTarget::Latest);
        assert!(latest.unwrap_err().to_string().contains("name a release"));
    }

    #[test]
    fn an_installer_binary_updates_itself() {
        let release = plan_for(
            "/home/op/.local/bin/archon",
            UpgradeTarget::Version("1.2.3".into()),
        )
        .unwrap();
        assert_eq!(release.command, Path::new("/home/op/.local/bin/archon"));
        assert_eq!(release.args, ["update", "--version", "v1.2.3"]);
        let latest = plan_for("/home/op/.local/bin/archon", UpgradeTarget::Latest).unwrap();
        assert_eq!(latest.args, ["update"]);
        let main = plan_for("/home/op/.local/bin/archon", UpgradeTarget::Main);
        assert!(main.unwrap_err().to_string().contains("built by cargo"));
    }

    #[test]
    fn without_a_cargo_directory_nothing_is_a_cargo_build() {
        let binary = Path::new("/home/op/.cargo/bin/aether");
        let step = plan(Program::Aether, &UpgradeTarget::Latest, binary, None).unwrap();
        assert_eq!(step.command, binary);
        assert!(!by_cargo(binary, None));
        assert!(!by_cargo(binary, Some(Path::new("/home/op/.cargo"))));
    }
}
