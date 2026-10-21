//! Where releases are published and what their assets are called. The
//! installer (`install.sh`) downloads the same assets from the same place.

use super::version::Version;

/// The public repository, `owner/name`; every address below derives from it.
const REPOSITORY: &str = "mengsig/archon";
/// Overrides the release address, for mirrors and tests.
pub const BASE_URL_ENV: &str = "ARCHON_INSTALL_BASE_URL";
/// Every binary a release archive ships; archond is `archon daemon run`.
pub const BINARIES: &[&str] = &["archon"];
/// The asset whose only line is the release's tag.
const VERSION_ASSET: &str = "VERSION";

/// The command that installs the latest release.
pub fn installer_command() -> String {
    format!("curl -fsSL https://raw.githubusercontent.com/{REPOSITORY}/main/install.sh | sh")
}

/// The address releases are downloaded from, without a trailing slash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    base: String,
}

impl Source {
    /// The address in `ARCHON_INSTALL_BASE_URL`, else the public releases.
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var(BASE_URL_ENV) {
            Ok(base) if !base.is_empty() => Self::new(&base),
            Ok(_) | Err(std::env::VarError::NotPresent) => {
                Self::new(&format!("https://github.com/{REPOSITORY}/releases"))
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                anyhow::bail!("{BASE_URL_ENV} is not valid UTF-8")
            }
        }
    }

    pub fn new(base: &str) -> anyhow::Result<Self> {
        let base = base.trim_end_matches('/');
        let rest = base
            .strip_prefix("https://")
            .or_else(|| base.strip_prefix("http://"));
        let plain = |rest: &str| !rest.is_empty() && !rest.contains(char::is_whitespace);
        anyhow::ensure!(
            rest.is_some_and(plain),
            "{BASE_URL_ENV} must be an https:// or http:// address, not `{base}`"
        );
        Ok(Self {
            base: base.to_owned(),
        })
    }

    /// Where the latest release names its own tag.
    pub fn latest_version_url(&self) -> String {
        format!("{}/latest/download/{VERSION_ASSET}", self.base)
    }

    /// The release page of `version`, where its notes are.
    pub fn notes_url(&self, version: &Version) -> String {
        format!("{}/tag/{}", self.base, version.tag())
    }

    /// One asset of the release tagged for `version`.
    pub fn asset_url(&self, version: &Version, asset: &str) -> String {
        assert!(
            !asset.is_empty() && !asset.contains('/'),
            "an asset is a file name: {asset:?}"
        );
        format!("{}/download/{}/{asset}", self.base, version.tag())
    }
}

/// The release target for an operating system and CPU as
/// `std::env::consts` name them, or `None` where no release is built.
pub fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

/// The release target this binary runs on.
pub fn current_target() -> anyhow::Result<&'static str> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    target_for(os, arch).ok_or_else(|| {
        anyhow::anyhow!(
            "no archon release is built for {os} on {arch}; build it from source instead"
        )
    })
}

/// The archive of a target, e.g. `archon-aarch64-apple-darwin.tar.gz`.
pub fn archive_name(target: &str) -> String {
    assert!(
        !target.is_empty() && !target.contains('/'),
        "a target is one name: {target:?}"
    );
    format!("archon-{target}.tar.gz")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_released_platform_has_its_archive_name() {
        let archive = |os, arch| target_for(os, arch).map(archive_name);
        assert_eq!(
            archive("linux", "x86_64").as_deref(),
            Some("archon-x86_64-unknown-linux-musl.tar.gz")
        );
        assert_eq!(
            archive("linux", "aarch64").as_deref(),
            Some("archon-aarch64-unknown-linux-musl.tar.gz")
        );
        assert_eq!(
            archive("macos", "aarch64").as_deref(),
            Some("archon-aarch64-apple-darwin.tar.gz")
        );
        assert_eq!(
            archive("macos", "x86_64").as_deref(),
            Some("archon-x86_64-apple-darwin.tar.gz")
        );
        assert_eq!(archive("windows", "x86_64"), None);
        assert_eq!(archive("linux", "riscv64"), None);
    }

    #[test]
    fn addresses_follow_the_github_release_layout() {
        let source = Source::new("https://example.test/releases/").unwrap();
        assert_eq!(
            source.latest_version_url(),
            "https://example.test/releases/latest/download/VERSION"
        );
        let version: Version = "1.2.3".parse().unwrap();
        assert_eq!(
            source.asset_url(&version, "archon-x86_64-apple-darwin.tar.gz"),
            "https://example.test/releases/download/v1.2.3/archon-x86_64-apple-darwin.tar.gz"
        );
        assert!(installer_command().contains(REPOSITORY));
    }

    #[test]
    fn only_http_addresses_are_release_sources() {
        for base in [
            "",
            "ftp://example.test",
            "example.test",
            "https://",
            "http://a b",
        ] {
            let error = Source::new(base).unwrap_err().to_string();
            assert!(error.contains(BASE_URL_ENV), "{base}: {error}");
        }
        assert!(Source::new("http://127.0.0.1:8080").is_ok());
    }
}
