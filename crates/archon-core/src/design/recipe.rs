//! A repository's saved preview recipe: how a live design session brought
//! the app's preview stack up, so the next session tries that first. It holds
//! commands, their order, ports, readiness checks and environment variable
//! names, and never a value: [`PreviewRecipe::check`] refuses what looks like one.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::QueueItemId;

pub const MAX_STEPS: usize = 12;
pub const MAX_ENV: usize = 32;
const MAX_SOURCE: usize = 200;
const MAX_COMMAND: usize = 1000;
const MAX_DIR: usize = 200;
const MAX_NOTES: usize = 1000;
const MAX_NAME: usize = 40;
const MAX_ENV_NAME: usize = 64;
/// The variable a Compose project name in a saved command must reference.
pub const COMPOSE_VARIABLE: &str = "COMPOSE_PROJECT_NAME";

/// Name segments of a variable or flag whose value is a secret.
const SECRET_WORDS: [&str; 11] = [
    "KEY",
    "APIKEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PASS",
    "CREDENTIAL",
    "CREDENTIALS",
    "AUTH",
    "PRIVATE",
];
/// Prefixes of well-known credential formats.
const TOKEN_PREFIXES: [&str; 11] = [
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "github_pat_",
    "glpat-",
    "sk-",
    "sk_live_",
    "xoxb-",
    "xoxp-",
    "AKIA",
];

/// How the preview stack came up, step by step, in start order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreviewRecipe {
    /// Where the route came from, such as `docs/dev-preview.md` or
    /// `package.json scripts.dev`.
    pub source: String,
    pub steps: Vec<RecipeStep>,
    /// Names of the environment variables the steps need; never values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// Anything the next session should know, such as the page to open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// One command of a recipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipeStep {
    /// The step's name; a service's PID goes in `.archon/services/<name>.pid`.
    pub name: String,
    /// The shell command, run from `dir`.
    pub command: String,
    /// The directory it runs from, relative to the worktree; its root when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    #[serde(default)]
    pub run: StepRun,
    /// The loopback TCP port the step listens on, when it is a service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// A loopback `http://` URL that answers without a server error once the step is up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<String>,
}

/// Whether a step keeps running or runs to completion.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StepRun {
    /// Keeps running: a dev server, a backend, a Compose stack.
    #[default]
    Service,
    /// Runs to completion first: an install, a migration, a seed.
    Once,
}

/// A recipe as Archon keeps it for a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SavedRecipe {
    pub recipe: PreviewRecipe,
    pub saved_at: DateTime<Utc>,
    /// The item whose design session saved it.
    pub item: QueueItemId,
}

impl PreviewRecipe {
    /// Refuse a malformed recipe, and one that seems to carry a secret value
    /// or names a Compose project other than through [`COMPOSE_VARIABLE`].
    pub fn check(&self) -> Result<(), String> {
        bounded_line("source", &self.source, MAX_SOURCE)?;
        if self.steps.is_empty() || self.steps.len() > MAX_STEPS {
            return Err(format!("a recipe has 1 to {MAX_STEPS} steps"));
        }
        let mut names = std::collections::BTreeSet::new();
        for step in &self.steps {
            step.check()?;
            if !names.insert(step.name.as_str()) {
                return Err(format!("step name {:?} is used twice", step.name));
            }
        }
        debug_assert_eq!(names.len(), self.steps.len(), "step names are unique");
        self.check_env()?;
        if let Some(notes) = &self.notes {
            bounded("notes", notes, MAX_NOTES)?;
            no_secret("notes", notes)?;
        }
        no_secret("source", &self.source)
    }

    fn check_env(&self) -> Result<(), String> {
        if self.env.len() > MAX_ENV {
            return Err(format!("at most {MAX_ENV} environment variable names"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for name in &self.env {
            if name.contains('=') {
                return Err(format!(
                    "env lists variable names only, never values: {:?}",
                    name.split('=').next().unwrap_or_default()
                ));
            }
            if !is_env_name(name) {
                return Err(format!("env name {name:?} is not a variable name"));
            }
            if !seen.insert(name.as_str()) {
                return Err(format!("env name {name} is listed twice"));
            }
        }
        Ok(())
    }

    /// Plain text for the agent and the operator.
    pub fn describe(&self) -> String {
        let mut lines = vec![format!("source: {}", self.source)];
        for (i, step) in self.steps.iter().enumerate() {
            let mut line = format!(
                "{}. {} ({}): {}",
                i + 1,
                step.name,
                step.run.word(),
                step.command
            );
            if let Some(dir) = &step.dir {
                line.push_str(&format!(" [in {dir}]"));
            }
            if let Some(port) = step.port {
                line.push_str(&format!(" port {port}"));
            }
            if let Some(ready) = &step.ready {
                line.push_str(&format!(" ready at {ready}"));
            }
            lines.push(line);
        }
        if !self.env.is_empty() {
            lines.push(format!("env names: {}", self.env.join(", ")));
        }
        if let Some(notes) = &self.notes {
            lines.push(format!("notes: {notes}"));
        }
        lines.join("\n")
    }
}

impl StepRun {
    fn word(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Once => "once",
        }
    }
}

impl SavedRecipe {
    pub fn describe(&self) -> String {
        format!(
            "saved {} by item {}\n{}",
            self.saved_at.format("%Y-%m-%d %H:%M UTC"),
            self.item,
            self.recipe.describe()
        )
    }
}

impl RecipeStep {
    fn check(&self) -> Result<(), String> {
        if !is_step_name(&self.name) {
            return Err(format!(
                "step name {:?} must be 1 to {MAX_NAME} of a-z, 0-9, - and _, starting with a letter or digit",
                self.name
            ));
        }
        let field = |what: &str| format!("step {} {what}", self.name);
        bounded(&field("command"), &self.command, MAX_COMMAND)?;
        no_secret(&field("command"), &self.command)?;
        compose_project_by_variable(&field("command"), &self.command)?;
        if let Some(dir) = &self.dir {
            bounded_line(&field("dir"), dir, MAX_DIR)?;
            let relative = std::path::Path::new(dir);
            if relative.is_absolute()
                || relative.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir | std::path::Component::Prefix(_)
                    )
                })
            {
                return Err(format!("{} must stay inside the worktree", field("dir")));
            }
        }
        if self.port == Some(0) {
            return Err(format!("{} must be nonzero", field("port")));
        }
        if let Some(ready) = &self.ready {
            crate::design::validate_preview_url(ready)
                .map_err(|e| format!("{}: {e}", field("ready")))?;
            no_secret(&field("ready"), ready)?;
        }
        Ok(())
    }
}

fn bounded(field: &str, text: &str, max: usize) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!("{field} is empty"));
    }
    if text.chars().count() > max {
        return Err(format!("{field} is longer than {max} characters"));
    }
    if text.contains('\0') {
        return Err(format!("{field} holds a NUL byte"));
    }
    Ok(())
}

fn bounded_line(field: &str, text: &str, max: usize) -> Result<(), String> {
    bounded(field, text, max)?;
    if text.contains(['\n', '\r']) {
        return Err(format!("{field} must be one line"));
    }
    Ok(())
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= MAX_ENV_NAME
}

fn is_step_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && name.len() <= MAX_NAME
}

/// The words of a command, split at whitespace and at the shell operators
/// that join commands, so `A=1;TOKEN=x` is two words.
fn shell_words(text: &str) -> Vec<&str> {
    text.split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '`'))
        .map(unquoted)
        .filter(|word| !word.is_empty())
        .collect()
}

/// A shell word without the quotes around it.
fn unquoted(word: &str) -> &str {
    word.trim_matches(|c| matches!(c, '"' | '\''))
}

/// A value that refers to a variable instead of spelling a value out. A path
/// is no reference: it cannot be told from a secret, so name it `$NAME` too.
fn is_reference(value: &str) -> bool {
    let value = unquoted(value);
    value.is_empty() || value.starts_with('$')
}

/// Words that end a name holding a secret even when glued to a prefix, as in
/// `PGPASSWORD` or `GITHUBTOKEN`.
const SECRET_SUFFIXES: [&str; 6] = [
    "PASSWORD",
    "PASSWD",
    "SECRET",
    "TOKEN",
    "APIKEY",
    "CREDENTIALS",
];

/// A secret word in the name, unless the name says it holds a file's place.
fn names_a_secret(name: &str, separator: char) -> bool {
    let upper = name.to_ascii_uppercase();
    let segments: Vec<&str> = upper.split(separator).collect();
    let secret = segments.iter().any(|s| {
        SECRET_WORDS.contains(s) || SECRET_SUFFIXES.iter().any(|suffix| s.ends_with(suffix))
    });
    secret
        && !segments
            .iter()
            .any(|s| matches!(*s, "FILE" | "PATH" | "DIR"))
}

/// Refuse a value spelled out for a secret-named variable or flag, a password
/// in a URL or a `user:password` pair, an `Authorization` header, or a
/// well-known token format.
fn no_secret(field: &str, text: &str) -> Result<(), String> {
    let words = shell_words(text);
    match secret_form(&words) {
        Some(what) => Err(format!(
            "{field} seems to hold a secret value ({what}); a recipe keeps names only, so refer to it as $NAME"
        )),
        None => Ok(()),
    }
}

/// What kind of spelled-out secret `words` hold, never the value itself.
fn secret_form(words: &[&str]) -> Option<String> {
    let next_is_value = |i: usize| {
        words
            .get(i + 1)
            .is_some_and(|next| !next.starts_with('-') && !is_reference(next))
    };
    let runs = |programs: &[&str]| {
        words
            .iter()
            .any(|w| programs.iter().any(|p| w.rsplit('/').next() == Some(*p)))
    };
    let takes_user = runs(&["curl", "wget", "http", "https"]);
    let takes_compact_password = runs(&["mysql", "mysqladmin", "mysqldump", "mariadb"]);
    for (i, word) in words.iter().enumerate() {
        if let Some((name, value)) = word.split_once('=') {
            let secret = match name.strip_prefix("--") {
                Some(flag) => names_a_secret(flag, '-'),
                None => is_env_name(name) && names_a_secret(name, '_'),
            };
            let user = takes_user && name == "--user" && user_password(value);
            if user || (secret && !is_reference(value)) {
                return Some(format!("{name}=…"));
            }
        } else if let Some(flag) = word.strip_prefix("--")
            && names_a_secret(flag, '-')
            && next_is_value(i)
        {
            return Some(format!("--{flag} …"));
        }
        if takes_user
            && matches!(*word, "-u" | "--user")
            && words.get(i + 1).is_some_and(|next| user_password(next))
        {
            return Some(format!("{word} user:password"));
        }
        if takes_compact_password
            && word.len() > 2
            && word.starts_with("-p")
            && !is_reference(&word[2..])
        {
            return Some("-p<password>".into());
        }
        if word.to_ascii_lowercase().ends_with("authorization:")
            && authorization_value(&words[i + 1..])
        {
            return Some("an Authorization header".into());
        }
        if url_password(word) {
            return Some("a password in a URL".into());
        }
        if is_token(word) {
            return Some("a credential token".into());
        }
    }
    None
}

/// `user:password` with the password spelled out.
fn user_password(value: &str) -> bool {
    value
        .split_once(':')
        .is_some_and(|(_, password)| !is_reference(password))
}

/// The words after `Authorization:` spell a credential out.
fn authorization_value(rest: &[&str]) -> bool {
    let scheme = rest.first().is_some_and(|w| {
        matches!(
            w.to_ascii_lowercase().as_str(),
            "bearer" | "basic" | "token"
        )
    });
    let value = if scheme { rest.get(1) } else { rest.first() };
    value.is_some_and(|v| !is_reference(v))
}

fn url_password(word: &str) -> bool {
    let Some((_, rest)) = word.split_once("://") else {
        return false;
    };
    // Up to the last `@`, not the first `/`: a raw `/` may sit in the password.
    let before_query = rest.split(['?', '#']).next().unwrap_or_default();
    let Some((userinfo, _)) = before_query.rsplit_once('@') else {
        return false;
    };
    user_password(userinfo)
}

/// A well-known credential format anywhere in the word, such as a token in
/// a URL's userinfo or after `=`.
fn is_token(word: &str) -> bool {
    if word.contains("-----BEGIN") {
        return true;
    }
    word.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .any(|piece| {
            TOKEN_PREFIXES.iter().any(|prefix| {
                piece
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.len() >= 16)
            })
        })
}

/// Compose options before the subcommand that take a separate value.
const COMPOSE_VALUE_OPTIONS: [&str; 7] = [
    "-f",
    "--file",
    "--profile",
    "--env-file",
    "--project-directory",
    "--ansi",
    "--progress",
];

/// A Compose project in a saved command is named only through
/// `$COMPOSE_PROJECT_NAME`, which each run sets to its own unique name.
fn compose_project_by_variable(field: &str, command: &str) -> Result<(), String> {
    let words = shell_words(command);
    let refers = |value: &str| {
        let value = unquoted(value);
        value == format!("${COMPOSE_VARIABLE}") || value == format!("${{{COMPOSE_VARIABLE}}}")
    };
    let assigned = words
        .iter()
        .filter_map(|w| w.strip_prefix(&format!("{COMPOSE_VARIABLE}=")));
    let named_by_option = words
        .iter()
        .enumerate()
        .filter(|(_, w)| **w == "compose" || w.ends_with("docker-compose"))
        .flat_map(|(i, _)| compose_project_options(&words[i + 1..]));
    let mut names = assigned.chain(named_by_option);
    if names.any(|value| !refers(value)) {
        return Err(format!(
            "{field} names a Compose project; write it as ${COMPOSE_VARIABLE}, which each run sets to its own name"
        ));
    }
    Ok(())
}

/// The project names Compose's global options give, read up to the
/// subcommand: `-p NAME`, `-pNAME`, `-p=NAME` and `--project-name[=]NAME`.
fn compose_project_options<'a>(options: &[&'a str]) -> Vec<&'a str> {
    let mut names = Vec::new();
    let mut i = 0;
    while let Some(&word) = options.get(i) {
        if !word.starts_with('-') {
            break;
        }
        if word == "-p" || word == "--project-name" {
            names.push(options.get(i + 1).copied().unwrap_or_default());
            i += 1;
        } else if let Some(value) = word.strip_prefix("--project-name=") {
            names.push(value);
        } else if let Some(value) = word.strip_prefix("-p")
            && !word.starts_with("--")
        {
            names.push(value.strip_prefix('=').unwrap_or(value));
        } else if COMPOSE_VALUE_OPTIONS.contains(&word) {
            i += 1;
        }
        i += 1;
    }
    debug_assert!(i <= options.len() + 1, "the scan stays within the options");
    names
}

#[cfg(test)]
mod tests;
