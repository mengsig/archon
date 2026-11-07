use super::*;
use crate::swe::arms::Model;

fn arm_set(dir: &Path) -> ArmSet {
    let build = |label: &str| {
        let archon = dir.join(format!("archon-{label}"));
        let aether = dir.join(format!("aether-{label}"));
        std::fs::write(&archon, "").unwrap();
        std::fs::write(&aether, "").unwrap();
        ArchonBuild::new(label, &archon, &aether).unwrap()
    };
    ArmSet::new(build("v1.4.9"), build("head"), vec![Model::Haiku]).unwrap()
}

fn request(arms: &ArmSet, split: Split, limit: Option<u32>) -> Request<'_> {
    Request {
        split,
        arms,
        repeats: 1,
        jobs: 3,
        load_max: 12.0,
        budget_usd: 90.0,
        limit,
    }
}

fn runner() -> Runner {
    Runner {
        python: "python3".into(),
        checkout: "/aether".into(),
    }
}

#[test]
fn the_runner_gets_the_set_split_arms_pairs_caps_and_run_id() {
    let dir = tempfile::tempdir().unwrap();
    let arms = arm_set(dir.path());
    let args = runner().args(
        &request(&arms, Split::Dev, None),
        Path::new("/s"),
        Some("archon-bench-1"),
    );
    let args: Vec<String> = args
        .iter()
        .map(|a| a.to_str().unwrap().to_owned())
        .collect();
    let bin = |name: &str| dir.path().join(name).display().to_string();
    let pair = |label: &str, build: &str| {
        format!(
            "{label}={},{}",
            bin(&format!("archon-{build}")),
            bin(&format!("aether-{build}"))
        )
    };
    let expected = [
        "/aether/evals/release_bench.py".to_owned(),
        "run".into(),
        "--set".into(),
        "swe-v1".into(),
        "--split".into(),
        "dev".into(),
        "--old".into(),
        bin("aether-v1.4.9"),
        "--new".into(),
        bin("aether-head"),
        "--arms".into(),
        "haiku/archon-old,haiku/archon-new,haiku/native".into(),
        "--archon-pair".into(),
        pair("old", "v1.4.9"),
        "--archon-pair".into(),
        pair("new", "head"),
    ];
    assert_eq!(args[..expected.len()], expected);
    let tail = &args[expected.len()..];
    assert_eq!(
        tail,
        [
            "--repeats",
            "1",
            "--jobs",
            "3",
            "--load-max",
            "12",
            "--budget",
            "90",
            "--work-root",
            "/s",
            "--run-id",
            "archon-bench-1"
        ]
    );
}

#[test]
fn counts_budgets_and_loads_out_of_range_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let arms = arm_set(dir.path());
    let good = request(&arms, Split::Dev, None);
    good.validate().unwrap();
    for (repeats, jobs) in [
        (0, 3),
        (MAX_REPEATS + 1, 3),
        (4_000_000_000, 3),
        (1, 0),
        (1, MAX_JOBS + 1),
    ] {
        let bad = Request {
            repeats,
            jobs,
            ..good.clone()
        };
        assert!(bad.validate().is_err(), "{repeats} repeats, {jobs} jobs");
    }
    for budget_usd in [0.0, -1.0, f64::INFINITY] {
        let bad = Request {
            budget_usd,
            ..good.clone()
        };
        assert!(bad.validate().is_err(), "budget {budget_usd}");
    }
    let bad = Request {
        load_max: f64::NAN,
        ..good
    };
    assert!(bad.validate().is_err());
}

#[test]
fn a_limit_reaches_the_runner_on_dev_and_is_refused_on_the_holdout() {
    let dir = tempfile::tempdir().unwrap();
    let arms = arm_set(dir.path());
    let dev = request(&arms, Split::Dev, Some(2));
    dev.validate().unwrap();
    let args = runner().args(&dev, Path::new("/s"), Some("r"));
    let at = args.iter().position(|a| a == "--limit").unwrap();
    assert_eq!(args[at + 1], "2");
    assert!(request(&arms, Split::Holdout, Some(2)).validate().is_err());
    assert!(request(&arms, Split::Dev, Some(0)).validate().is_err());
    let holdout = runner().args(&request(&arms, Split::Holdout, None), Path::new("/s"), None);
    assert!(
        !holdout.iter().any(|a| a == "--run-id" || a == "--limit"),
        "the holdout is never named or limited"
    );
}

#[test]
fn runner_arm_names_follow_the_runners_archon_pair_labels() {
    let key = |model, harness| ArmKey { model, harness };
    assert_eq!(
        runner_arm(key(Model::Haiku, Harness::Native)),
        "haiku/native"
    );
    assert_eq!(
        runner_arm(key(Model::Luna, Harness::Old)),
        "luna/archon-old"
    );
    assert_eq!(
        runner_arm(key(Model::Luna, Harness::New)),
        "luna/archon-new"
    );
}
