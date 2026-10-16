//! Public release-check contract, with no network or GitHub mutations.
mod common;

use archon_core::testing::write_script;
use common::Env;
use serde_json::{Value, json};
use std::process::Output;

const TARGETS: &[&str] = &[
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
];

fn assets(product: &str) -> Vec<Value> {
    let mut names = vec!["VERSION".to_owned(), "SHA256SUMS".to_owned()];
    for target in TARGETS {
        names.push(format!("{product}-{target}.tar.gz"));
        names.push(format!("{product}-{target}.tar.gz.sha256"));
    }
    names
        .into_iter()
        .map(|name| json!({"name": name, "size": 100}))
        .collect()
}

fn fixture(product: &str) -> Env {
    let env = Env::new();
    for (name, value) in [
        ("tags", json!([[{"name": "v1.9.0"}], [{"name": "v1.10.0"}]])),
        (
            "releases",
            json!([[release(product, "v1.9.0")], [release(product, "v1.10.0")]]),
        ),
        ("latest", json!({"tag_name": "v1.10.0"})),
        ("runs", json!({"workflow_runs": [{"id": 42}]})),
        (
            "jobs",
            json!([{"jobs": [{"started_at": "2026-10-16T00:00:00Z", "runner_id": 5}]}]),
        ),
    ] {
        std::fs::write(env.path(name), value.to_string()).unwrap();
    }
    write_script(
        &env.path("gh"),
        r#"#!/bin/sh
set -eu
[ "$1" = api ] || exit 10
case "$2" in
  */tags\?*) file=tags ;;
  */releases\?*) file=releases ;;
  */releases/latest) file=latest ;;
  */actions/workflows/ci.yml/runs\?status=completed\&per_page=1) file=runs ;;
  */actions/runs/42/jobs\?*) file=jobs ;;
  *) echo "unexpected endpoint $2" >&2; exit 11 ;;
esac
case "$file" in
  tags|releases|jobs) [ "$3 $4" = '--paginate --slurp' ] || exit 12 ;;
esac
cat "$(dirname "$0")/$file"
"#,
    );
    std::fs::write(
        env.path("config/config.toml"),
        format!(
            "[aether]\nmode = 'shared'\n[github]\nbinary = '{}'\n",
            env.path("gh").display()
        ),
    )
    .unwrap();
    env
}

fn release(product: &str, tag: &str) -> Value {
    json!({"tag_name": tag, "draft": false, "prerelease": false, "assets": assets(product)})
}

fn check(env: &Env, extra: &[&str]) -> (Output, Value) {
    let mut args = vec!["--json", "release", "check"];
    args.extend_from_slice(extra);
    let output = env.command(&args).output().unwrap();
    let value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("{e}: {output:?}"));
    (output, value)
}

#[test]
fn healthy_paginated_history_uses_numeric_semver_and_supports_aether() {
    for product in ["archon", "aether"] {
        let env = fixture(product);
        let (output, value) = check(&env, &[&format!("mengsig/{product}")]);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(value["problems"], json!([]));
    }
}

#[test]
fn missing_release_assets_and_wrong_latest_are_all_reported() {
    let env = fixture("archon");
    std::fs::write(env.path("tags"), r#"[[{"name":"v1.11.0"}]]"#).unwrap();
    let mut latest = release("archon", "v1.10.0");
    latest["assets"] = json!([]);
    std::fs::write(
        env.path("releases"),
        json!([[release("archon", "v1.9.0"), latest]]).to_string(),
    )
    .unwrap();
    std::fs::write(env.path("latest"), r#"{"tag_name":"v1.9.0"}"#).unwrap();
    let (output, value) = check(&env, &[]);
    assert_eq!(output.status.code(), Some(1));
    let problems = value["problems"].to_string();
    for expected in [
        "v1.11.0: tag has no published release",
        "missing or empty asset VERSION",
        "missing or empty asset SHA256SUMS",
        ".tar.gz.sha256",
        "Latest is v1.9.0, highest stable semver is v1.10.0",
    ] {
        assert!(problems.contains(expected), "{problems}");
    }
}

#[test]
fn ci_without_runner_jobs_is_reported() {
    let env = fixture("archon");
    for jobs in [
        json!([]),
        json!([{"started_at": "2026-10-16", "runner_id": 0}]),
    ] {
        std::fs::write(env.path("jobs"), json!([{"jobs": jobs}]).to_string()).unwrap();
        let (output, value) = check(&env, &[]);
        assert_eq!(output.status.code(), Some(1));
        assert!(
            value["problems"]
                .to_string()
                .contains("CI run 42 never started")
        );
    }
}

#[test]
fn pending_release_requires_tag_and_complete_local_assets() {
    let env = fixture("archon");
    std::fs::write(env.path("tags"), r#"[[{"name":"v1.11.0"}]]"#).unwrap();
    let dist = env.path("dist");
    std::fs::create_dir(&dist).unwrap();
    let args = [
        "mengsig/archon",
        "--pending-tag",
        "v1.11.0",
        "--assets",
        dist.to_str().unwrap(),
    ];
    let (output, value) = check(&env, &args);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        value["problems"]
            .to_string()
            .contains("missing or empty asset")
    );
    for asset in assets("archon") {
        std::fs::write(dist.join(asset["name"].as_str().unwrap()), "fixture").unwrap();
    }
    std::fs::write(dist.join("VERSION"), "v1.11.0\n").unwrap();
    let (output, value) = check(&env, &args);
    assert!(output.status.success(), "{value}");
    assert_eq!(value["problems"], json!([]));
    std::fs::write(dist.join("VERSION"), "v1.10.0\n").unwrap();
    let (output, value) = check(&env, &args);
    assert_eq!(output.status.code(), Some(1));
    assert!(value["problems"].to_string().contains("VERSION contains"));
}

#[test]
fn drafts_do_not_satisfy_tags_and_prereleases_do_not_replace_latest() {
    let env = fixture("archon");
    let mut draft = release("archon", "v1.10.0");
    draft["draft"] = json!(true);
    let mut prerelease = release("archon", "v2.0.0-rc.1");
    prerelease["prerelease"] = json!(true);
    std::fs::write(
        env.path("releases"),
        json!([[release("archon", "v1.9.0"), draft, prerelease]]).to_string(),
    )
    .unwrap();
    std::fs::write(env.path("latest"), r#"{"tag_name":"v1.9.0"}"#).unwrap();
    let (output, value) = check(&env, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        value["problems"],
        json!(["v1.10.0: tag has no published release"])
    );
}

#[test]
fn api_failure_and_malformed_json_fail_closed() {
    let env = fixture("archon");
    for script in [
        "#!/bin/sh\necho denied >&2\nexit 1\n",
        "#!/bin/sh\necho not-json\n",
    ] {
        write_script(&env.path("gh"), script);
        let output = env.command(&["release", "check"]).output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn doctor_checks_both_repositories() {
    let env = fixture("archon");
    let output = env.command(&["doctor", "--json"]).output().unwrap();
    let checks: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    let releases: Vec<_> = checks.iter().filter(|c| c["check"] == "releases").collect();
    assert_eq!(releases.len(), 2);
    assert!(
        releases[0]["detail"]
            .as_str()
            .unwrap()
            .contains("mengsig/archon")
    );
    assert!(
        releases[1]["detail"]
            .as_str()
            .unwrap()
            .contains("mengsig/aether")
    );
}

#[test]
fn workflow_gates_draft_creation_without_ignoring_failure() {
    let workflow = include_str!("../../../.github/workflows/release.yml");
    let gate = workflow
        .find("      - name: gate the release on release health")
        .unwrap();
    let publish = workflow
        .find("      - name: create the draft release")
        .unwrap();
    assert!(gate < publish);
    let step = &workflow[gate..publish];
    assert!(step.contains("release check \"$GITHUB_REPOSITORY\""));
    assert!(step.contains("--pending-tag \"$GITHUB_REF_NAME\" --assets dist"));
    assert!(!step.contains("continue-on-error") && !step.contains("||"));
    assert!(workflow.contains("      actions: read"));
}

#[test]
fn workflow_shell_stops_before_publication_on_failed_check() {
    let env = fixture("archon");
    let dist = env.path("dist");
    let runner = env.path("runner");
    std::fs::create_dir(&dist).unwrap();
    std::fs::create_dir(&runner).unwrap();
    write_script(
        &env.path("archon"),
        "#!/bin/sh\nexec \"$TEST_ARCHON\" \"$@\"\n",
    );
    let packed = std::process::Command::new("tar")
        .args([
            "-czf",
            "dist/archon-x86_64-unknown-linux-musl.tar.gz",
            "archon",
        ])
        .current_dir(env.path(""))
        .status()
        .unwrap();
    assert!(packed.success());
    let workflow = include_str!("../../../.github/workflows/release.yml");
    let step = workflow
        .split("      - name: gate the release on release health\n")
        .nth(1)
        .unwrap();
    let run = step.split("        run: |\n").nth(1).unwrap();
    let script = run
        .lines()
        .take_while(|line| line.starts_with("          "))
        .map(|line| line.strip_prefix("          ").unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let output = std::process::Command::new("sh")
        .args(["-ec", &format!("{script}\nprintf published > published\n")])
        .current_dir(env.path(""))
        .env("RUNNER_TEMP", &runner)
        .env("GITHUB_REPOSITORY", "mengsig/archon")
        .env("GITHUB_REF_NAME", "v1.10.0")
        .env("ARCHON_CONFIG_DIR", env.path("config"))
        .env("TEST_ARCHON", env!("CARGO_BIN_EXE_archon"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("missing or empty asset"));
    assert!(!env.path("published").exists());
}

#[test]
fn a_non_version_latest_is_a_health_failure() {
    let env = fixture("archon");
    std::fs::write(env.path("latest"), r#"{"tag_name":"nightly"}"#).unwrap();
    let (output, value) = check(&env, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        value["problems"],
        json!(["Latest is nightly, highest stable semver is v1.10.0"])
    );
}

#[test]
fn queued_runs_do_not_hide_the_latest_completed_runs_health() {
    let env = fixture("archon");
    let script = std::fs::read_to_string(env.path("gh")).unwrap();
    let script = script.replace(
        "case \"$2\" in",
        r#"case "$2" in
  */actions/workflows/ci.yml/runs\?per_page=1)
    echo '{"workflow_runs":[{"id":43,"status":"queued"}]}'
    exit 0 ;;"#,
    );
    write_script(&env.path("gh"), &script);
    let (output, value) = check(&env, &[]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(value["problems"], json!([]));
    std::fs::write(env.path("jobs"), r#"[{"jobs":[]}]"#).unwrap();
    let (output, value) = check(&env, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        value["problems"]
            .to_string()
            .contains("CI run 42 never started")
    );
}
