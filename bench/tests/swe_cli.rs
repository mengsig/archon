//! `archon-bench swe` end to end against a stub of Aether's runner
//! (`evals/release_bench.py run`, as of aether 73f66333). On the holdout it
//! never prints holdout content and keeps no copy of it: the stub leaks a
//! canary on stdout, on stderr, in its work root, and in Aether's own gate
//! reasons and stop reason. On dev it passes both builds through and the gate
//! decides.

use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};

const CANARY: &str = "sealed-canary-astropy__astropy-424242";

/// Writes its aggregate where the real runner does: a dev run under its
/// `--run-id`, a holdout run under a name of its own.
const FAKE_RUNNER: &str = r#"printf '%s\n' "$@" > "$FAKE_ARGS"
checkout=$(cd "$(dirname "$0")/.." && pwd)
while [ $# -gt 0 ]; do
  case "$1" in
    --split) split=$2 ;;
    --run-id) run_id=$2 ;;
    --work-root) work=$2 ;;
  esac
  shift
done
echo "instance $CANARY: diff --git a/x b/x"
echo "prompt for $CANARY" >&2
mkdir -p "$work/runs" && printf '%s' "$CANARY" > "$work/runs/transcript.txt"
runs="$checkout/target/release-bench"
if [ "$split" = dev ]; then out="$runs/runs/$run_id"; mkdir -p "$out"
else mkdir -p "$runs/holdout" && out=$(mktemp -d "$runs/holdout/run-XXXXXX"); fi
if [ "$FAKE_WRITE" = 1 ]; then cp "$FAKE_AGGREGATE" "$out/aggregate.json"; fi
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
        let set = checkout.join("bench/release-set/swe-v1");
        std::fs::create_dir_all(&set).unwrap();
        let pin = "0".repeat(64);
        let manifest = json!({"format": "swe/1", "splits": {"dev": {
            "dataset": "SWE-bench/SWE-bench_Verified", "revision": "78f471bf",
            "instances": {"a__b-1": pin, "c__d-2": pin, "e__f-3": pin}}}});
        std::fs::write(set.join("manifest.json"), manifest.to_string()).unwrap();
        std::fs::write(checkout.join("evals/release_bench.py"), FAKE_RUNNER).unwrap();
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

    fn run(&self, aggregate: Option<&Value>, exit: u32) -> Output {
        self.run_split("holdout", aggregate, exit, &[])
    }

    /// Run `swe run`; the stub writes `aggregate` (if any) and exits `exit`.
    fn run_split(
        &self,
        split: &str,
        aggregate: Option<&Value>,
        exit: u32,
        extra: &[&str],
    ) -> Output {
        let aggregate_file = self.data_home.join("fake-aggregate.json");
        let text = aggregate.map_or(String::new(), Value::to_string);
        std::fs::write(&aggregate_file, text).unwrap();
        let bin = |name: &str| self.bins.join(name);
        std::process::Command::new(env!("CARGO_BIN_EXE_archon-bench"))
            .args(["swe", "run", "--split", split, "--models", "haiku"])
            .args(["--python", "/bin/sh", "--budget-usd", "90"])
            .args(extra)
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
            .env("FAKE_WRITE", if aggregate.is_some() { "1" } else { "0" })
            .env("FAKE_ARGS", self.data_home.join("runner-args.txt"))
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

/// An arm in the runner's schema, `aether-release-bench-aggregate/1`.
fn arm(name: &str, n: u32, passed: u32) -> Value {
    let (model, harness) = name.split_once('/').unwrap();
    json!({"model": model, "harness": harness, "trials": n, "infra": 0, "n": n, "passed": passed,
           "pass_rate": f64::from(passed) / f64::from(n), "pass_ci": [0.2, 0.6],
           "cost_usd_total": 12.5, "unpriced": 0, "cost_per_solved_usd": 1.1,
           "wall_seconds_total": 30000.0, "wall_per_solved_seconds": 2700.0})
}

/// A pair in the runner's schema: `b` only the baseline passed, `c` only the candidate.
fn pair(tasks: u32, baseline: &str, candidate: &str, b: u32, c: u32) -> Value {
    json!({"model": "haiku", "baseline": baseline, "candidate": candidate, "tasks": tasks,
           "pairs": tasks, "b": b, "c": c, "p_one_sided": 0.6, "pass_diff": [0.0, -0.2, 0.2]})
}

fn runner_output(split: &str, tasks: u32, passed: [u32; 3], pairs: [Value; 3]) -> Value {
    let [native, old, new] = passed;
    json!({
        "schema": "aether-release-bench-aggregate/1",
        "set": {"name": "swe-v1", "split": split, "sha256": "c".repeat(64), "tasks": tasks, "excluded": 0},
        "settings": {"confidence": 0.95, "accuracy_margin": 0.05, "native_alpha": 0.05},
        "arms": {"haiku/native": arm("haiku/native", tasks, native),
                 "haiku/archon-old": arm("haiku/archon-old", tasks, old),
                 "haiku/archon-new": arm("haiku/archon-new", tasks, new)},
        "pairs": pairs,
        "gates": {"haiku": {"release": null,
                            "native": {"haiku/archon-new": {"verdict": "FAIL", "reasons": [CANARY]}}}},
        "overall": "FAIL",
        "stop_reason": CANARY,
    })
}

/// A holdout of 25 tasks; Aether's gate reasons and stop reason carry the canary.
fn aggregate() -> Value {
    runner_output(
        "holdout",
        25,
        [11, 11, 11],
        [
            pair(25, "haiku/native", "haiku/archon-old", 2, 2),
            pair(25, "haiku/native", "haiku/archon-new", 3, 3),
            pair(25, "haiku/archon-old", "haiku/archon-new", 2, 2),
        ],
    )
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
    let output = sandbox.run(Some(&aggregate()), 0);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_silent(&output);
    let args = std::fs::read_to_string(sandbox.data_home.join("runner-args.txt")).unwrap();
    assert!(args.contains("--split\nholdout\n"), "{args}");
    assert!(
        !args.contains("--run-id") && !args.contains("--limit"),
        "{args}"
    );
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
    leaky["instances"] =
        json!([{"task": CANARY, "arm": "haiku/archon-new", "repeat": 0, "passed": true}]);
    let output = sandbox.run(Some(&leaky), 0);
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
    let output = sandbox.run(None, 3);
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
            "--budget-usd",
            "1",
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

#[test]
fn out_of_range_counts_are_refused_as_errors_before_any_estimate() {
    let sandbox = Sandbox::new();
    for extra in [
        ["--holdout-instances", "0"],
        ["--holdout-instances", "5000"],
        ["--repeats", "4000000000"],
        ["--jobs", "0"],
    ] {
        let args = [&extra[..], &["--dry-run"]].concat();
        let output = sandbox.run_split("holdout", Some(&aggregate()), 0, &args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{extra:?}: {stderr}");
        assert!(!stderr.contains("panicked"), "{extra:?}: {stderr}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("estimate"), "{extra:?}: {stdout}");
    }
    assert!(sandbox.kept_files().is_empty());
}

/// Two dev tasks: new solves both, native only a__b-1, old only c__d-2.
fn dev_aggregate() -> Value {
    let verdict =
        |task, arm, passed| json!({"task": task, "arm": arm, "repeat": 0, "passed": passed});
    let mut value = runner_output(
        "dev",
        2,
        [1, 1, 2],
        [
            pair(2, "haiku/native", "haiku/archon-old", 1, 1),
            pair(2, "haiku/native", "haiku/archon-new", 0, 1),
            pair(2, "haiku/archon-old", "haiku/archon-new", 0, 1),
        ],
    );
    value["instances"] = json!([
        verdict("a__b-1", "haiku/archon-new", true),
        verdict("c__d-2", "haiku/archon-new", true),
        verdict("a__b-1", "haiku/native", true),
        verdict("c__d-2", "haiku/native", false),
        verdict("a__b-1", "haiku/archon-old", false),
        verdict("c__d-2", "haiku/archon-old", true),
    ]);
    value
}

fn saved(sandbox: &Sandbox, name: &str) -> PathBuf {
    let found: Vec<PathBuf> = sandbox
        .kept_files()
        .into_iter()
        .filter(|f| f.ends_with(name))
        .collect();
    assert_eq!(found.len(), 1, "one {name}: {found:?}");
    found[0].clone()
}

fn gate(sandbox: &Sandbox, holdout: Option<&Path>) -> Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_archon-bench"));
    command
        .args([
            "swe",
            "gate",
            "--old-label",
            "v1.4.9",
            "--models",
            "haiku",
            "--dev",
        ])
        .arg(saved(sandbox, "dev-aggregate.json"))
        .arg("--out")
        .arg(sandbox.results.join("gate"))
        .env("XDG_DATA_HOME", &sandbox.data_home);
    if let Some(holdout) = holdout {
        command.arg("--holdout").arg(holdout);
    }
    command.output().unwrap()
}

#[test]
fn a_dev_run_hands_the_runner_both_builds_and_the_limit_and_keeps_no_verdicts() {
    let sandbox = Sandbox::new();
    // Exit 1 is the runner's own non-PASS verdict, after it wrote its aggregate.
    let output = sandbox.run_split("dev", Some(&dev_aggregate()), 1, &["--limit", "2"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("dev set: SWE-bench/SWE-bench_Verified@78f471bf"),
        "{stdout}"
    );
    assert!(
        stdout.contains("SWE dev split: 2 instances, 1 repeat(s)"),
        "{stdout}"
    );
    assert!(stdout.contains("estimate (priors): 6 trials"), "{stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("exited exit status: 1 after writing its aggregate"),
        "{stderr}"
    );
    assert!(
        stdout.contains("| haiku | dev | 2 | 1 | 0 | 1.000 | pass |"),
        "{stdout}"
    );
    let args = std::fs::read_to_string(sandbox.data_home.join("runner-args.txt")).unwrap();
    let bin = |name: &str| sandbox.bins.join(name).display().to_string();
    for expected in [
        "run\n--set\nswe-v1\n--split\ndev\n".to_owned(),
        format!(
            "--old\n{}\n--new\n{}\n",
            bin("aether-old"),
            bin("aether-new")
        ),
        "--arms\nhaiku/archon-old,haiku/archon-new,haiku/native\n".to_owned(),
        format!(
            "--archon-pair\nold={},{}\n",
            bin("archon-old"),
            bin("aether-old")
        ),
        format!(
            "--archon-pair\nnew={},{}\n",
            bin("archon-new"),
            bin("aether-new")
        ),
        "--jobs\n3\n--load-max\n12\n--budget\n90\n".to_owned(),
        "--limit\n2\n--run-id\narchon-bench-".to_owned(),
    ] {
        assert!(args.contains(&expected), "{expected:?} in\n{args}");
    }
    let kept = std::fs::read_to_string(saved(&sandbox, "dev-aggregate.json")).unwrap();
    assert!(
        !kept.contains("a__b-1"),
        "the saved dev aggregate keeps no instance ids"
    );
}

#[test]
fn the_gate_fails_without_a_holdout_and_passes_with_both_splits_not_worse() {
    let sandbox = Sandbox::new();
    assert!(
        sandbox
            .run_split("dev", Some(&dev_aggregate()), 0, &[])
            .status
            .success()
    );
    let incomplete = gate(&sandbox, None);
    assert!(
        !incomplete.status.success(),
        "a missing holdout cannot pass"
    );
    let markdown = std::fs::read_to_string(sandbox.results.join("gate/gate.md")).unwrap();
    assert!(
        markdown.contains("### Never worse than native\n"),
        "{markdown}"
    );
    assert!(markdown.contains("### Never worse than the best release (v1.4.9)\n"));
    assert!(markdown.contains("(no holdout aggregate)"), "{markdown}");
    assert!(sandbox.run(Some(&aggregate()), 0).status.success());
    let holdout = saved(&sandbox, "holdout-aggregate.json");
    let complete = gate(&sandbox, Some(&holdout));
    assert!(
        complete.status.success(),
        "{}",
        String::from_utf8_lossy(&complete.stdout)
    );
    let json: Value = serde_json::from_str(
        &std::fs::read_to_string(sandbox.results.join("gate/gate.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(json["comparisons"].as_array().unwrap().len(), 4);
    assert!(
        json["comparisons"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["verdict"] == "pass")
    );
}
