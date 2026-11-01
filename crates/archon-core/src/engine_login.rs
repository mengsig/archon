//! An engine whose login failed or expired, as its agent's failure words it.
//! Archon holds such an engine instead of failing its stages (ARCHITECTURE §7.3).

/// What an agent failure says when its engine's login failed. Aether words a
/// model error `<Class>: <message>`, as in `Auth: token expired`.
const LOGIN_WORDS: [&str; 7] = [
    "auth:",
    "authenticat",
    "unauthorized",
    "not logged in",
    "login expired",
    "oauth",
    "credential",
];

/// Whether an agent failure's `error` says its engine's login failed.
pub fn is_login_failure(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    LOGIN_WORDS.iter().any(|word| error.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_or_expired_login_is_recognised_however_it_is_worded() {
        for error in [
            "model error from claude-code/opus: Auth: token expired",
            "Failed to authenticate. API Error: 401",
            "401 Unauthorized",
            "claude: Not logged in. Please run /login",
            "OAuth token has expired",
            "invalid credentials",
        ] {
            assert!(is_login_failure(error), "{error}");
        }
        for error in [
            "engine died",
            "RateLimit: slow down",
            "the author is unknown",
            "",
        ] {
            assert!(!is_login_failure(error), "{error}");
        }
    }
}
