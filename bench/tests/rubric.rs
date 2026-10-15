//! The keyword rubrics must accept equivalent wording and reject near misses.

use std::path::Path;
use std::process::Command;

#[test]
fn review_seeded_bug_rubric_unit_tests_pass() {
    let graders = Path::new(env!("CARGO_MANIFEST_DIR")).join("graders");
    let output = Command::new("python3")
        .args(["-m", "unittest", "-q", "test_rubric"])
        .env("PYTHONPATH", &graders)
        .current_dir(&graders)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
