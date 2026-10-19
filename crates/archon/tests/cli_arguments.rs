//! Bad arguments are refused before archond is contacted, naming the value
//! and never the parser behind it.

mod common;

use common::Env;

/// The stderr of `archon <args>`, which must fail with exit status 1.
fn refusal(env: &Env, args: &[&str]) -> String {
    let out = env.command(args).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(1), "archon {args:?}: {stderr}");
    for internal in ["ULID", "DirectiveId", "update_item"] {
        assert!(!stderr.contains(internal), "{internal} leaked: {stderr}");
    }
    stderr
}

#[test]
fn a_directive_id_error_names_the_value() {
    let env = Env::new();
    let stderr = refusal(&env, &["autonomy", "clear", "nope"]);
    assert!(
        stderr.contains("\"nope\" is not a directive id"),
        "{stderr}"
    );
}

#[test]
fn a_cost_range_reaching_too_far_back_is_refused() {
    let env = Env::new();
    let stderr = refusal(&env, &["cost", "--since", "99999d"]);
    assert!(
        stderr.contains("\"99999d\" reaches back too far"),
        "{stderr}"
    );
}

#[test]
fn every_bad_extension_gets_the_same_message() {
    let env = Env::new();
    for bad in ["0", "99999999999", "1441", "soon"] {
        let args = ["queue", "steer", "qi_1", "--extend", bad];
        let out = env.command(&args).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let expected = format!("{bad:?} is not an extension of 1 to 1440 minutes");
        assert_eq!(out.status.code(), Some(2), "{bad}: {stderr}");
        assert!(stderr.contains(&expected), "{bad}: {stderr}");
    }
}

#[test]
fn a_negative_budget_is_refused_with_its_range() {
    let env = Env::new();
    let brief = env.path("brief.md");
    std::fs::write(&brief, "Read the cache design.").unwrap();
    let brief = brief.to_str().unwrap();
    for budget in ["-1", "0"] {
        let args = [
            "queue",
            "add",
            "--title",
            "t",
            "--pipeline",
            "ask",
            "--brief-file",
            brief,
            "--budget-usd",
            budget,
        ];
        let stderr = refusal(&env, &args);
        assert!(
            stderr.contains("a budget must be a finite amount of USD greater than zero"),
            "{budget}: {stderr}"
        );
    }
    let steer = refusal(&env, &["queue", "steer", "qi_1", "--budget-usd", "-1"]);
    assert!(steer.contains("a budget must be"), "{steer}");
}
