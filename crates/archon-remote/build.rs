//! Embeds the web app: every runtime file under `web/`, as written. Tooling,
//! tests and dependencies are left out (docs/remote.md, "Assets").

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const SKIPPED_DIRS: [&str; 6] = [
    "dev",
    "tests",
    "scripts",
    "node_modules",
    "test-results",
    "playwright-report",
];
const SKIPPED_FILES: [&str; 2] = ["package.json", "package-lock.json"];

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let web = manifest.join("web");
    println!("cargo:rerun-if-changed={}", web.display());
    let mut files = Vec::new();
    collect(&web, &web, &mut files);
    files.sort();
    assert!(
        files.iter().any(|(path, _)| path == "/index.html"),
        "web/index.html must exist"
    );
    let mut table = String::from("&[\n");
    for (route, file) in &files {
        writeln!(
            table,
            "    ({route:?}, include_bytes!({:?})),",
            file.display()
        )
        .unwrap();
    }
    table.push(']');
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets it"));
    std::fs::write(out.join("assets.rs"), table).expect("writing assets.rs");
}

/// Every shipped file under `dir`, as (`/route`, absolute path).
fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("reading a web/ entry");
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let kind = entry.file_type().expect("reading a web/ entry's type");
        // Rerun when a file is added to or removed from any directory.
        println!("cargo:rerun-if-changed={}", path.display());
        if kind.is_dir() {
            if !SKIPPED_DIRS.contains(&name.as_str()) {
                collect(root, &path, files);
            }
        } else if kind.is_file() && shipped(&name) {
            let relative = path.strip_prefix(root).expect("under web/");
            let route = format!("/{}", relative.to_string_lossy().replace('\\', "/"));
            files.push((route, path));
        }
    }
}

fn shipped(name: &str) -> bool {
    !(SKIPPED_FILES.contains(&name)
        || name.ends_with(".test.js")
        || name.ends_with(".spec.js")
        || name.ends_with(".md")
        || name.contains(".config."))
}
