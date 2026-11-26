//! What a recipe's text may hold: no spelled-out secret, and no Compose
//! project named other than through [`COMPOSE_VARIABLE`]. Commands are read
//! with the shell's quoting, and also as raw words, so neither reading hides one.

use super::{COMPOSE_VARIABLE, is_env_name};

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
/// Characters that end a shell word and the command it belongs to.
const OPERATORS: [char; 9] = [';', '&', '|', '(', ')', '`', '<', '>', '\n'];
/// Characters that end a command in the raw reading; `(` and backticks stay
/// inside it so an expansion after `compose` is seen there.
const SEPARATORS: [char; 4] = [';', '&', '|', '\n'];
/// Compose options whose value, separate or after `=`, is a file, a profile
/// or a directory: a double-quoted expansion there cannot become an option.
const VALUE_OPTIONS: [&str; 5] = [
    "-f",
    "--file",
    "--profile",
    "--env-file",
    "--project-directory",
];
/// How deep scripts nested in words, as in `sh -c "sh -c '…'"`, are read.
const MAX_NESTING: usize = 8;
/// Check a step's command: it parses as shell words, spells out no secret
/// and names a Compose project only through [`COMPOSE_VARIABLE`].
pub(super) fn check_command(field: &str, command: &str) -> Result<(), String> {
    let commands = shell_commands(command).map_err(|e| format!("{field} {e}"))?;
    no_secret(field, command)?;
    let parsed: Vec<&str> = commands
        .iter()
        .flatten()
        .flat_map(|word| word.text.split_whitespace())
        .collect();
    if let Some(what) = secret_form(&parsed) {
        return Err(refusal(field, &what));
    }
    let mut readings = nested_commands(field, &commands)?;
    readings.extend(commands);
    let mut texts: Vec<Vec<String>> = readings
        .iter()
        .map(|words| words.iter().map(|word| word.text.clone()).collect())
        .collect();
    texts.extend(raw_commands(command));
    compose_project_by_variable(field, &texts)?;
    compose_expands_safely(field, &readings)
}

/// The commands of scripts nested in words, as in `sh -c '…'`, read with the
/// shell's quoting; a nested script naming Compose that cannot be read is refused.
fn nested_commands(field: &str, commands: &[Vec<Word>]) -> Result<Vec<Vec<Word>>, String> {
    let unreadable = || {
        format!(
            "{field} runs Compose in a nested script whose quoting cannot be read; make it parse and name the project only as ${COMPOSE_VARIABLE}"
        )
    };
    let mut nested = Vec::new();
    let mut level: Vec<String> = commands.iter().flatten().map(|w| w.text.clone()).collect();
    for _ in 0..MAX_NESTING {
        let mut next = Vec::new();
        for word in level.iter().filter(|word| may_be_script(word)) {
            match shell_commands(word) {
                Ok(inner) => {
                    next.extend(inner.iter().flatten().map(|w| w.text.clone()));
                    nested.extend(inner);
                }
                Err(_) if word.contains("compose") => return Err(unreadable()),
                Err(_) => {}
            }
        }
        level = next;
    }
    if level
        .iter()
        .any(|word| may_be_script(word) && word.contains("compose"))
    {
        return Err(unreadable());
    }
    Ok(nested)
}

/// A word the shell would read differently were it run as a script; reading
/// one strips at least one of these characters, so the nesting ends.
fn may_be_script(word: &str) -> bool {
    word.chars()
        .any(|c| c.is_whitespace() || OPERATORS.contains(&c) || matches!(c, '"' | '\'' | '\\'))
}

/// Refuse a value spelled out in free text, read as raw words.
pub(super) fn no_secret(field: &str, text: &str) -> Result<(), String> {
    match secret_form(&raw_words(text)) {
        Some(what) => Err(refusal(field, &what)),
        None => Ok(()),
    }
}

fn refusal(field: &str, what: &str) -> String {
    format!(
        "{field} seems to hold a secret value ({what}); a recipe keeps names only, so refer to it as $NAME"
    )
}

/// Words split at whitespace and operators with only their outer quotes
/// trimmed: quoting that glues a literal to a reference stays visible.
fn raw_words(text: &str) -> Vec<&str> {
    text.split(|c: char| c.is_whitespace() || OPERATORS.contains(&c))
        .map(unquoted)
        .filter(|word| !word.is_empty())
        .collect()
}

/// A shell word without the quotes around it.
fn unquoted(word: &str) -> &str {
    word.trim_matches(|c| matches!(c, '"' | '\''))
}

#[derive(Clone, Copy, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

/// A shell word as its own level reads it, and how that level reads the
/// `$` and backticks in it.
#[derive(Clone, Default)]
struct Word {
    text: String,
    /// Expanded outside quotes, so the value is split into more words.
    splits: bool,
    /// Expanded inside double quotes, so the value stays one word.
    quoted: bool,
    /// Kept literal by quotes or a backslash, so a script re-reading the word
    /// would expand it.
    literal: bool,
}

impl Word {
    fn expands(&self) -> bool {
        self.splits || self.quoted
    }
}

/// Add `c`, read under `quote`, to the word; an escaped character is read as
/// if single-quoted.
fn push(word: &mut Option<Word>, c: char, quote: Quote) {
    let word = word.get_or_insert_default();
    if matches!(c, '$' | '`') {
        match quote {
            Quote::None => word.splits = true,
            Quote::Double => word.quoted = true,
            Quote::Single => word.literal = true,
        }
    }
    word.text.push(c);
}

/// The command's simple commands, each a list of words, read with the
/// shell's quotes and backslashes; an unclosed quote is an error.
fn shell_commands(text: &str) -> Result<Vec<Vec<Word>>, String> {
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word: Option<Word> = None;
    let mut quote = Quote::None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
            (Quote::Single, c) => push(&mut word, c, Quote::Single),
            (Quote::Double | Quote::None, '\\') if chars.next_if_eq(&'\n').is_some() => {}
            (Quote::Double, '\\') => match chars.next_if(|n| matches!(n, '"' | '\\' | '$' | '`')) {
                Some(escaped) => push(&mut word, escaped, Quote::Single),
                None => push(&mut word, '\\', Quote::Double),
            },
            (Quote::Double, c) => push(&mut word, c, Quote::Double),
            (Quote::None, '\'' | '"') => {
                quote = if c == '"' {
                    Quote::Double
                } else {
                    Quote::Single
                };
                word.get_or_insert_default();
            }
            (Quote::None, '\\') => push(&mut word, chars.next().unwrap_or('\\'), Quote::Single),
            (Quote::None, c) if c.is_whitespace() || OPERATORS.contains(&c) => {
                if c == '`' {
                    // The substitution's output joins this command as more words.
                    word.get_or_insert_default().splits = true;
                }
                words.extend(word.take());
                if OPERATORS.contains(&c) && !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            (Quote::None, c) => push(&mut word, c, Quote::None),
        }
    }
    if quote != Quote::None {
        return Err("has a quote that is never closed".into());
    }
    words.extend(word);
    if !words.is_empty() {
        commands.push(words);
    }
    debug_assert!(commands.iter().all(|c| !c.is_empty()), "no empty command");
    Ok(commands)
}

/// A value that is a whole variable reference, `$NAME` or `${NAME}`, or
/// empty. Anything else, a path or `$(…)` included, could spell a secret out.
fn is_reference(value: &str) -> bool {
    let value = unquoted(value);
    let Some(name) = value.strip_prefix('$') else {
        return value.is_empty();
    };
    let name = name
        .strip_prefix('{')
        .and_then(|n| n.strip_suffix('}'))
        .unwrap_or(name);
    is_env_name(name)
}

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

/// What kind of spelled-out secret `words` hold, never the value itself: a
/// secret-named variable or flag, a password in a URL or a `user:password`
/// pair, an `Authorization` header, or a well-known token format.
fn secret_form(words: &[&str]) -> Option<String> {
    let runs = |programs: &[&str]| {
        words
            .iter()
            .any(|w| programs.iter().any(|p| w.rsplit('/').next() == Some(*p)))
    };
    let takes_user = runs(&["curl", "wget", "http", "https"]);
    // The short option that takes `user:password`: curl's -u, HTTPie's -a.
    let user_letter = (runs(&["curl"]), 'u');
    let auth_letter = (runs(&["http", "https"]), 'a');
    let takes_compact_password = runs(&["mysql", "mysqladmin", "mysqldump", "mariadb"]);
    for (i, word) in words.iter().enumerate() {
        if let Some(what) = secret_option(words, i) {
            return Some(what);
        }
        if takes_user
            && *word == "--user"
            && words.get(i + 1).is_some_and(|next| user_password(next))
        {
            return Some("--user user:password".into());
        }
        let short_user = [user_letter, auth_letter]
            .into_iter()
            .any(|(runs, letter)| {
                runs && short_option_value(words, i, letter).is_some_and(user_password)
            });
        if short_user {
            return Some("a short option's user:password".into());
        }
        let compact = word.strip_prefix("-p").filter(|p| !p.is_empty());
        if takes_compact_password && compact.is_some_and(|password| !is_reference(password)) {
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

/// `NAME=value`, `--flag=value` or `--flag value` with a secret name and a
/// value spelled out, and `--user=user:password`.
fn secret_option(words: &[&str], i: usize) -> Option<String> {
    let word = words[i];
    if let Some((name, value)) = word.split_once('=') {
        let secret = match name.strip_prefix("--") {
            Some(flag) => names_a_secret(flag, '-') || (flag == "user" && user_password(value)),
            None => is_env_name(name) && names_a_secret(name, '_'),
        };
        return (secret && !is_reference(value)).then(|| format!("{name}=…"));
    }
    let flag = word.strip_prefix("--")?;
    let spelled_out = words
        .get(i + 1)
        .is_some_and(|next| !next.starts_with('-') && !is_reference(next));
    (names_a_secret(flag, '-') && spelled_out).then(|| format!("--{flag} …"))
}

/// The value of short option `letter` at `words[i]`, alone (`-u VALUE`) or in
/// a cluster (`-uVALUE`, `-suVALUE`, `-su VALUE`).
fn short_option_value<'a>(words: &[&'a str], i: usize, letter: char) -> Option<&'a str> {
    let cluster = words[i].strip_prefix('-').filter(|c| !c.starts_with('-'))?;
    let (flags, value) = cluster.split_once(letter)?;
    if !flags.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    if value.is_empty() {
        words.get(i + 1).copied()
    } else {
        Some(value)
    }
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

/// A password in a URL's userinfo, including one whose raw `/` ends the
/// authority early (`app:pa/ss@host`): a host's port is digits only.
fn url_password(word: &str) -> bool {
    let Some((_, rest)) = word.split_once("://") else {
        return false;
    };
    let before_query = rest.split(['?', '#']).next().unwrap_or_default();
    let authority = before_query.split('/').next().unwrap_or_default();
    if let Some((userinfo, _)) = authority.rsplit_once('@') {
        return user_password(userinfo);
    }
    let path = &before_query[authority.len()..];
    let not_a_port = |port: &str| port.is_empty() || !port.chars().all(|c| c.is_ascii_digit());
    path.contains('@')
        && !authority.ends_with(']')
        && authority
            .rsplit_once(':')
            .is_some_and(|(_, port)| not_a_port(port))
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

/// The command's simple commands as raw words with every quote and backslash
/// dropped, so a Compose call inside a nested `sh -c '…'` script is read too.
fn raw_commands(text: &str) -> Vec<Vec<String>> {
    joined_lines(text)
        .split(SEPARATORS)
        .map(|segment| {
            segment
                .split_whitespace()
                .map(|word| word.replace(['"', '\'', '\\'], ""))
                .filter(|word| !word.is_empty())
                .collect()
        })
        .collect()
}

/// The text with every line continuation joined, at any nesting: a newline
/// after one or more backslashes is dropped with them.
fn joined_lines(text: &str) -> String {
    let mut joined = String::with_capacity(text.len());
    for c in text.chars() {
        if c != '\n' || !joined.ends_with('\\') {
            joined.push(c);
            continue;
        }
        while joined.ends_with('\\') {
            joined.pop();
        }
    }
    debug_assert!(
        joined.len() <= text.len(),
        "joining only removes characters"
    );
    joined
}

/// The index of the word that runs Compose (`compose`, `docker-compose`,
/// `podman-compose`, or a path to one), if the command has one.
fn compose_position<W: AsRef<str>>(words: &[W]) -> Option<usize> {
    words
        .iter()
        .position(|word| word.as_ref().ends_with("compose"))
}

impl AsRef<str> for Word {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

/// Deny by default: after `compose`, a word expands only as the run's project
/// or as the double-quoted value of a [`VALUE_OPTIONS`] option, and no outer
/// shell expands into a nested Compose script, so none can spell out `-p NAME`.
fn compose_expands_safely(field: &str, commands: &[Vec<Word>]) -> Result<(), String> {
    for words in commands {
        let into_script = words
            .iter()
            .any(|w| w.expands() && w.text.contains("compose") && may_be_script(&w.text));
        if into_script {
            return Err(format!(
                "{field} expands a variable into a nested script that runs Compose; single-quote the script so only it expands ${COMPOSE_VARIABLE}"
            ));
        }
        let Some(compose) = compose_position(words) else {
            continue;
        };
        let after = &words[compose..];
        if (1..after.len()).any(|i| !expands_safely(after, i)) {
            return Err(format!(
                "{field} expands a variable or command after compose; there only ${COMPOSE_VARIABLE} and a double-quoted value of {} may be expanded, as in --profile \"$NAME\"",
                VALUE_OPTIONS.join(", ")
            ));
        }
    }
    Ok(())
}

/// `words[i]`, after `compose` at 0, expands nothing, names the run's project,
/// or is a double-quoted value that a [`VALUE_OPTIONS`] option consumes.
fn expands_safely(words: &[Word], i: usize) -> bool {
    assert!(i > 0 && i < words.len(), "a word after compose");
    let word = &words[i];
    if !word.expands() && !word.literal {
        return true;
    }
    if refers_to_run_project(project_option_value(&word.text).unwrap_or(&word.text)) {
        return true;
    }
    if !word.quoted || word.splits || word.literal {
        return false;
    }
    let attached = word
        .text
        .split_once('=')
        .is_some_and(|(option, _)| VALUE_OPTIONS.contains(&option));
    attached || takes_the_next_word(words, i - 1)
}

/// `words[j]` is a [`VALUE_OPTIONS`] option that Compose reads as an option,
/// as it follows `compose` or a word that is no option, so it takes the next word.
fn takes_the_next_word(words: &[Word], j: usize) -> bool {
    let Some(previous) = j.checked_sub(1).map(|p| &words[p]) else {
        return false;
    };
    let option = &words[j];
    let plain = !option.expands() && !option.literal;
    plain
        && VALUE_OPTIONS.contains(&option.text.as_str())
        && (j == 1 || !previous.text.starts_with('-'))
}

/// A Compose project in a saved command is named only through
/// `$COMPOSE_PROJECT_NAME`, which each run sets to its own unique name.
fn compose_project_by_variable(field: &str, commands: &[Vec<String>]) -> Result<(), String> {
    let assignment = format!("{COMPOSE_VARIABLE}=");
    for words in commands {
        let foreign_assignment = words
            .iter()
            .filter_map(|w| w.strip_prefix(&assignment))
            .any(|value| !refers_to_run_project(value));
        if foreign_assignment {
            return Err(format!(
                "{field} names a Compose project; write it as ${COMPOSE_VARIABLE}, which each run sets to its own name"
            ));
        }
        let Some(compose) = compose_position(words) else {
            continue;
        };
        if !project_options_name_the_run(&words[compose + 1..]) {
            return Err(format!(
                "{field} passes Compose a -p or --project-name other than ${COMPOSE_VARIABLE}, which each run sets to its own name; publish ports in the compose file rather than with `run -p`"
            ));
        }
    }
    Ok(())
}

fn refers_to_run_project(value: &str) -> bool {
    value == format!("${COMPOSE_VARIABLE}") || value == format!("${{{COMPOSE_VARIABLE}}}")
}

/// Deny by default: every word after `compose` that is a project option,
/// `-p`, `-pNAME`, `-p=NAME` or `--project-name[=]NAME`, must name the run's
/// project, wherever it stands, so no option parsing can be outflanked.
fn project_options_name_the_run(words: &[String]) -> bool {
    let mut i = 0;
    while let Some(word) = words.get(i).map(String::as_str) {
        let value = if word == "-p" || word == "--project-name" {
            i += 1;
            Some(words.get(i).map(String::as_str).unwrap_or_default())
        } else {
            project_option_value(word)
        };
        if value.is_some_and(|value| !refers_to_run_project(value)) {
            return false;
        }
        i += 1;
    }
    debug_assert!(i >= words.len(), "every word after compose was read");
    true
}

/// The project a single-word option names: `-pNAME`, `-p=NAME` or
/// `--project-name=NAME`.
fn project_option_value(word: &str) -> Option<&str> {
    if let Some(value) = word.strip_prefix("--project-name=") {
        return Some(value);
    }
    let value = word.strip_prefix("-p")?;
    Some(value.strip_prefix('=').unwrap_or(value))
}
