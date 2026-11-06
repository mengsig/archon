//! `archon-bench swe run --split holdout` never prints holdout content and
//! keeps no copy of it. A fake Aether runner leaks a canary on stdout, on
//! stderr, in its scratch directory and, in one case, in its aggregate.

use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};

const CANARY: &str = "sealed-canary-astropy__astropy-424242";

const FAKE_RUNNER: &str = r#"out=""
while [ $# -gt 0 ]; do
  if [ "$1" = --aggregate-out ]; then out=$2; fi
  shift
done
echo "instance $CANARY: diff --git a/x b/x"
echo "prompt for $CANARY" >&2
printf '%s' "$CANARY" > "$(dirname "$out")/transcript.txt"
cp "$FAKE_AGGREGATE" "$out"
exit "$FAKE_EXIT"
"#;

struct Sandbox {
    _root: tempfile::TempDir,
    data_home: PathBuf,
    checkout: PathBuf,
    bins: PathBuf,
    work: PathBuf,
    results: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let dir = |name: &str| {
            let path = root.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        };
        let (data_home, checkout, bins) = (dir("data"), dir("aether"), dir("bins"));
        std::fs::create_dir_all(checkout.join("evals")).unwrap();
        std::fs::write(checkout.join("evals/swe_bench.py"), FAKE_RUNNER).unwrap();
        for name in ["archon-old", "aether-old", "archon-new", "aether-new"] {
            std::fs::write(bins.join(name), "").unwrap();
        }
        let (work, results) = (root.path().join("work"), root.path().join("results"));
        Self {
            data_home,
            checkout,
            bins,
            work,
            results,
            _root: root,
        }
    }

    fn run(&self, aggregate: &Value, exit: u32) -> Output {
        let aggregate_file = self.data_home.join("fake-aggregate.json");
        std::fs::write(&aggregate_file, aggregate.to_string()).unwrap();
        let bin = |name: &str| self.bins.join(name);
        std::process::Command::new(env!("CARGO_BIN_EXE_archon-bench"))
            .args([
                "swe", "run", "--split", "holdout", "--models", "haiku", "--python", "/bin/sh",
            ])
            .arg("--aether-checkout")
            .arg(&self.checkout)
            .args(["--old-label", "v1.4.9", "--new-label", "head"])
            .arg("--old-archon")
            .arg(bin("archon-old"))
            .arg("--old-aether")
            .arg(bin("aether-old"))
            .arg("--new-archon")
            .arg(bin("archon-new"))
            .arg("--new-aether")
            .arg(bin("aether-new"))
            .arg("--work-dir")
            .arg(&self.work)
            .arg("--results-dir")
            .arg(&self.results)
            .env("XDG_DATA_HOME", &self.data_home)
            .env("CANARY", CANARY)
            .env("FAKE_AGGREGATE", &aggregate_file)
            .env("FAKE_EXIT", exit.to_string())
            .output()
            .unwrap()
    }

    /// Every file archon-bench left in its work and results directories.
    fn kept_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for root in [&self.work, &self.results] {
            collect(root, &mut files);
        }
        files
    }
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, files);
        } else {
            files.push(path);
        }
    }
}

fn aggregate() -> Value {
    let arm = json!({"n": 25, "passed": 11, "infra": 0, "cost_usd": 12.5, "wall_s": 30000.0});
    json!({
        "schema": "swe-aggregate/1", "split": "holdout",
        "arms": {"haiku/old": arm, "haiku/new": arm, "haiku/native": arm},
        "pairs": [
            {"a": "haiku/new", "b": "haiku/native", "n": 25, "a_only": 3, "b_only": 3},
            {"a": "haiku/new", "b": "haiku/old", "n": 25, "a_only": 2, "b_only": 2},
        ],
    })
}

fn assert_silent(output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains(CANARY),
        "stdout leaks holdout content:\n{stdout}"
    );
    assert!(
        !stderr.contains(CANARY),
        "stderr leaks holdout content:\n{stderr}"
    );
}

fn assert_no_copy(sandbox: &Sandbox) {
    for file in sandbox.kept_files() {
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains(CANARY),
            "{} keeps holdout content",
            file.display()
        );
        assert!(
            !text.contains("instance"),
            "{} keeps an instance field",
            file.display()
        );
    }
}

#[test]
fn a_holdout_run_prints_only_aggregates_and_keeps_only_the_validated_aggregate() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&aggregate(), 0);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_silent(&output);
    assert_no_copy(&sandbox);
    let kept = sandbox.kept_files();
    assert_eq!(kept.len(), 1, "only the aggregate is kept: {kept:?}");
    assert!(kept[0].ends_with("holdout-aggregate.json"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("### Never worse than native"), "{stdout}");
    assert!(
        stdout.contains("| haiku | holdout | 25 | 3 | 3 |"),
        "{stdout}"
    );
}

#[test]
fn a_holdout_aggregate_with_instances_is_refused_unprinted_and_unkept() {
    let sandbox = Sandbox::new();
    let mut leaky = aggregate();
    leaky["instances"] = json!([{"instance_id": CANARY, "arm": "haiku/new", "resolved": true}]);
    let output = sandbox.run(&leaky, 0);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("per-instance"));
    assert_silent(&output);
    assert_no_copy(&sandbox);
    assert!(
        sandbox.kept_files().is_empty(),
        "nothing is kept: {:?}",
        sandbox.kept_files()
    );
}

#[test]
fn a_failing_holdout_runner_is_reported_without_its_output() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&aggregate(), 3);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("discards its output"), "{stderr}");
    assert_silent(&output);
    assert!(
        sandbox.kept_files().is_empty(),
        "nothing is kept: {:?}",
        sandbox.kept_files()
    );
}

#[test]
fn a_run_pointed_into_the_holdout_store_is_refused_before_the_runner_starts() {
    let sandbox = Sandbox::new();
    let store = sandbox.data_home.join("bench-holdout/swe-v1");
    std::fs::create_dir_all(&store).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_archon-bench"))
        .args([
            "swe",
            "run",
            "--split",
            "dev",
            "--dry-run",
            "--old-label",
            "a",
            "--new-label",
            "b",
        ])
        .args([
            "--old-archon",
            "x",
            "--old-aether",
            "x",
            "--new-archon",
            "y",
            "--new-aether",
            "y",
        ])
        .arg("--aether-checkout")
        .arg(&store)
        .env("XDG_DATA_HOME", &sandbox.data_home)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("sealed holdout store"));
    assert!(sandbox.kept_files().is_empty());
}
