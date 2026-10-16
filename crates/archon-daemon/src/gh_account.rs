//! The GitHub account a repository's `gh` calls and pushes run as, when the
//! operator named one (ARCHITECTURE §8.7). Its token is read from `gh` for
//! each call and handed only to that call's own process: to `gh` for
//! github.com, to git for a push over https to github.com. It is never
//! logged, stored, shown in an error or put in archond's own environment.

use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::repo::{GH_HOST, GhUser, github_slug};

use crate::worktree::stack;

/// Shortest and longest token taken from `gh auth token`.
const MIN_TOKEN_LEN: usize = 8;
const MAX_TOKEN_LEN: usize = 1024;
/// What a token is shown as wherever it would have appeared.
const REDACTED: &str = "[token]";
/// Every variable through which `gh` would pick another token or host.
pub(crate) const AMBIENT: [&str; 5] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_HOST",
];

/// One login's token, for one call. No `Display`, and `Debug` hides it.
pub(crate) struct Token {
    user: GhUser,
    secret: String,
    /// `Authorization: Basic` credentials, which carry it encoded.
    basic: String,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("user", &self.user.as_str())
            .field("secret", &REDACTED)
            .finish()
    }
}

impl Token {
    /// The token `gh auth token` printed for `user`; the error never
    /// repeats what it printed.
    pub(crate) fn read(user: &GhUser, printed: &str) -> Result<Self, ArchonError> {
        let secret = printed.trim();
        let shaped = (MIN_TOKEN_LEN..=MAX_TOKEN_LEN).contains(&secret.len())
            && secret.bytes().all(|b| b.is_ascii_graphic());
        if !shaped {
            return Err(ArchonError::Protocol(format!(
                "`gh auth token` printed no usable token for {user} on {GH_HOST}"
            )));
        }
        let basic = base64(format!("{user}:{secret}").as_bytes());
        assert!(
            basic.len() > secret.len(),
            "the user and the token, encoded"
        );
        Ok(Self {
            user: user.clone(),
            secret: secret.to_owned(),
            basic,
        })
    }

    pub(crate) fn user(&self) -> &GhUser {
        &self.user
    }

    /// What `gh` is told, beside dropping every [`AMBIENT`] variable: this
    /// token, which `gh` sends to github.com and no other host.
    pub(crate) fn gh_env(&self) -> [(&'static str, &str); 2] {
        assert!(!self.secret.is_empty(), "a read token");
        [("GH_TOKEN", &self.secret), ("GH_HOST", GH_HOST)]
    }

    /// Git settings for a push as this user: its credentials sent to
    /// `https://github.com/` only, never on to where a redirect points.
    pub(crate) fn push_settings(&self) -> Vec<(String, String)> {
        let header = format!("Authorization: Basic {}", self.basic);
        let settings = vec![
            (format!("http.https://{GH_HOST}/.extraHeader"), header),
            ("http.followRedirects".to_owned(), "false".to_owned()),
        ];
        assert!(settings.iter().all(|(key, _)| key.starts_with("http.")));
        settings
    }

    /// `text` with the token, plain or encoded, replaced wherever it occurs.
    pub(crate) fn redact(&self, text: &str) -> String {
        assert!(self.secret.len() >= MIN_TOKEN_LEN, "checked when read");
        text.replace(&self.basic, REDACTED)
            .replace(&self.secret, REDACTED)
    }
}

/// Where a push as `user` goes: `repo`'s `origin` as an https URL of
/// github.com. `Err` names why none can be, for the operator.
pub(crate) fn push_url(repo: &Path, user: &GhUser) -> Result<Result<String, String>, ArchonError> {
    assert!(repo.is_absolute(), "a registered repository");
    let origin = match stack::origin_url(repo) {
        Ok(Some(url)) => url,
        Ok(None) => return Ok(Err("the repository has no `origin` remote".to_owned())),
        Err(ArchonError::Invalid(why)) => return Ok(Err(why)),
        Err(error) => return Err(error),
    };
    let Some(slug) = github_slug(&origin) else {
        return Ok(Err(format!(
            "`origin` is {origin:?}, not a {GH_HOST} repository over https or ssh, so it \
             cannot be reached as {user}, the GitHub login this repository runs as"
        )));
    };
    let url = format!("https://{GH_HOST}/{slug}.git");
    assert!(url.starts_with("https://github.com/"), "only github.com");
    Ok(Ok(url))
}

/// Standard base64, padded, as HTTP basic credentials need.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            let sextet = (triple >> (18 - 6 * i)) & 0x3f;
            let shown = i <= chunk.len();
            encoded.push(if shown {
                ALPHABET[sextet as usize] as char
            } else {
                '='
            });
        }
    }
    assert_eq!(encoded.len() % 4, 0, "padded to whole quanta");
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::daemon_git::DaemonGit;

    const SECRET: &str = "gho_FAKEfakeFAKE0123456789";

    fn token() -> Token {
        let user: GhUser = "devfake".parse().unwrap();
        Token::read(&user, &format!("{SECRET}\n")).unwrap()
    }

    #[test]
    fn base64_matches_the_standard_encoding() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
            ("user:pass", "dXNlcjpwYXNz"),
        ];
        for (plain, encoded) in cases {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain:?}");
        }
    }

    #[test]
    fn a_token_shows_nowhere_and_is_redacted_from_any_text() {
        let token = token();
        let shown = format!("{token:?}");
        assert!(
            !shown.contains(SECRET) && shown.contains("devfake"),
            "{shown}"
        );
        let encoded = base64(format!("devfake:{SECRET}").as_bytes());
        let said = format!("fatal: {SECRET} was refused; header Basic {encoded}");
        let redacted = token.redact(&said);
        assert!(!redacted.contains(SECRET) && !redacted.contains(&encoded));
        assert_eq!(redacted, "fatal: [token] was refused; header Basic [token]");
    }

    #[test]
    fn only_a_plain_token_is_taken_and_the_refusal_does_not_repeat_it() {
        let user: GhUser = "devfake".parse().unwrap();
        for printed in [
            "",
            "  \n",
            "short",
            "two words",
            &"x".repeat(MAX_TOKEN_LEN + 1),
        ] {
            let error = Token::read(&user, printed).unwrap_err().to_string();
            assert!(error.contains("devfake"), "{error}");
            assert!(printed.trim().is_empty() || !error.contains(printed.trim()));
        }
    }

    #[test]
    fn a_push_carries_the_token_to_github_over_https_only() {
        let token = token();
        let settings = token.push_settings();
        let (key, header) = &settings[0];
        assert_eq!(key, "http.https://github.com/.extraHeader");
        let encoded = base64(format!("devfake:{SECRET}").as_bytes());
        assert_eq!(header, &format!("Authorization: Basic {encoded}"));
        assert!(settings.contains(&("http.followRedirects".into(), "false".into())));
        assert_eq!(
            token.gh_env(),
            [("GH_TOKEN", SECRET), ("GH_HOST", "github.com")]
        );
    }

    fn repo_with_origin(origin: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        let git = |args: &[&str]| {
            DaemonGit::sealed_in(&path)
                .args(args.iter().copied())
                .text()
        };
        git(&["init", "--quiet"]).unwrap();
        git(&["remote", "add", "origin", origin]).unwrap();
        (dir, path)
    }

    #[test]
    fn git_sends_the_header_to_github_com_over_https_and_nowhere_else() {
        let token = token();
        let (_dir, path) = repo_with_origin("https://github.com/o/r.git");
        let header = |url: &str| {
            let git = DaemonGit::sealed_in(&path)
                .secret_config(&token.push_settings())
                .args(["config", "--get-urlmatch", "http.extraheader", url]);
            let shown = format!("{git:?}");
            assert!(
                !shown.contains(SECRET) && !shown.contains(&token.basic),
                "{shown}"
            );
            git.output().unwrap().stdout_text().unwrap()
        };
        assert_eq!(
            header("https://github.com/o/r.git").trim(),
            format!("Authorization: Basic {}", token.basic)
        );
        for elsewhere in [
            "http://github.com/o/r.git",
            "https://github.com.evil.example/o/r.git",
            "https://ghe.example.com/o/r.git",
            "https://evil.example/github.com/o/r.git",
        ] {
            assert_eq!(header(elsewhere).trim(), "", "{elsewhere}");
        }
    }

    #[test]
    fn a_push_as_a_login_goes_only_to_github_com_over_https() {
        let user: GhUser = "devfake".parse().unwrap();
        for (origin, url) in [
            ("https://github.com/o/r.git", "https://github.com/o/r.git"),
            ("git@github.com:o/r.git", "https://github.com/o/r.git"),
            ("ssh://git@github.com/o/r", "https://github.com/o/r.git"),
        ] {
            let (_dir, path) = repo_with_origin(origin);
            assert_eq!(
                push_url(&path, &user).unwrap(),
                Ok(url.to_owned()),
                "{origin}"
            );
        }
        for origin in ["https://ghe.example.com/o/r.git", "/srv/git/r.git"] {
            let (_dir, path) = repo_with_origin(origin);
            let why = push_url(&path, &user).unwrap().unwrap_err();
            assert!(why.contains("devfake") && why.contains(origin), "{why}");
        }
    }
}
