use std::process::Command;

fn rdny() -> &'static str {
    env!("CARGO_BIN_EXE_rdny")
}

#[test]
fn process_cleanup_does_not_remove_new_registration_or_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let xdg = temp.path().join("xdg");
    let state_dir = temp.path().join("custom").join("../state");

    let status = Command::new(rdny())
        .arg("register-sample")
        .env("RDNY_TEST_HELPER", "1")
        .env("RDNY_STATE_DIR", &state_dir)
        .env("XDG_STATE_HOME", &xdg)
        .status()
        .unwrap();
    assert!(status.success());

    let status = Command::new(rdny())
        .arg("replace-sample")
        .env("RDNY_TEST_HELPER", "1")
        .env("RDNY_STATE_DIR", &state_dir)
        .env("XDG_STATE_HOME", &xdg)
        .status()
        .unwrap();
    assert!(status.success());

    let output = Command::new(rdny())
        .arg("cleanup")
        .env("RDNY_TEST_HELPER", "1")
        .env("RDNY_STATE_DIR", &state_dir)
        .env("XDG_STATE_HOME", &xdg)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = Command::new(rdny())
        .arg("list")
        .env("RDNY_STATE_DIR", temp.path().join("state"))
        .env("XDG_STATE_HOME", &xdg)
        .current_dir("/")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("replacement") || stdout.contains("missing") || stdout.contains("dead")
    );
}

#[test]
fn lifecycle_persistence_failures_are_recoverable() {
    let temp = tempfile::tempdir().unwrap();
    let xdg = temp.path().join("xdg");
    for failed in ["state.json", "registry.json"] {
        let dir = temp.path().join(failed);
        let status = Command::new(rdny())
            .arg("register-sample")
            .env("RDNY_TEST_HELPER", "1")
            .env("RDNY_STATE_DIR", &dir)
            .env("XDG_STATE_HOME", &xdg)
            .env("RDNY_TEST_FAIL_WRITE", failed)
            .status()
            .unwrap();
        assert!(!status.success());

        let status = Command::new(rdny())
            .arg("register-sample")
            .env("RDNY_TEST_HELPER", "1")
            .env("RDNY_STATE_DIR", &dir)
            .env("XDG_STATE_HOME", &xdg)
            .status()
            .unwrap();
        assert!(status.success());
    }
}
