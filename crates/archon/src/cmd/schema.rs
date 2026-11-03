//! `archon schema`: the daemon protocol as a JSON Schema, the file the web
//! frontend vendors (CONTRACTS.md §1.3).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use archon_core::protocol::schema::{SCHEMA_PATH, schema_text};

#[derive(Debug, clap::Args)]
pub struct SchemaArgs {
    /// Write the schema to this file instead of stdout, e.g. the committed
    /// `crates/archon-core/protocol.schema.json`.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

/// The same bytes either way; `--json` changes nothing, as it is JSON.
pub fn run(args: &SchemaArgs) -> anyhow::Result<ExitCode> {
    let text = schema_text();
    assert!(text.ends_with('\n'), "the schema ends in a newline");
    let Some(out) = &args.out else {
        print!("{text}");
        return Ok(ExitCode::SUCCESS);
    };
    assert!(!out.as_os_str().is_empty(), "clap refuses an empty --out");
    std::fs::write(out, &text)
        .with_context(|| format!("writing the protocol schema to {}", out.display()))?;
    eprintln!(
        "Wrote {} ({} bytes); the committed copy is {SCHEMA_PATH}.",
        out.display(),
        text.len()
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_writes_exactly_the_schema_text() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("schema.json");
        let args = SchemaArgs {
            out: Some(out.clone()),
        };
        assert_eq!(run(&args).unwrap(), ExitCode::SUCCESS);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), schema_text());
    }

    #[test]
    fn an_unwritable_out_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("missing").join("schema.json");
        let error = run(&SchemaArgs { out: Some(out) }).unwrap_err();
        assert!(format!("{error:#}").contains("missing"), "{error:#}");
    }
}
