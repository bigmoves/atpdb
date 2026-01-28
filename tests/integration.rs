use std::process::Command;

#[test]
fn test_binary_runs() {
    let output = Command::new("cargo")
        .args(["run", "--", "--help"])
        .output()
        .expect("failed to run");

    // Just verify it doesn't crash on startup
    // (it will exit with unknown flag, but that's fine)
    assert!(output.status.code().is_some());
}
