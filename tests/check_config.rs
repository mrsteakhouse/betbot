#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

fn betbot() -> Command {
    Command::new(env!("CARGO_BIN_EXE_betbot"))
}

#[test]
fn check_config_accepts_example() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/config.yaml");
    let out = betbot()
        .arg("--config")
        .arg(&path)
        .arg("--check-config")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn check_config_rejects_invalid_with_exit_2() {
    let dir = std::env::temp_dir().join(format!("betbot-check-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bad.yaml");
    std::fs::write(&path, "tenants: []\n").unwrap();
    let out = betbot()
        .arg("--config")
        .arg(&path)
        .arg("--check-config")
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn check_config_rejects_unknown_field_like_existing_secret() {
    // Guards the chart contract: `existingSecret` must never reach the app config.
    let dir = std::env::temp_dir().join(format!("betbot-check-config-u-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bad2.yaml");
    std::fs::write(
        &path,
        "tenants:\n  - name: a\n    existingSecret: s\n    platform: { type: fake, fixture: f.yaml }\n",
    )
    .unwrap();
    let out = betbot()
        .arg("--config")
        .arg(&path)
        .arg("--check-config")
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(out.status.code(), Some(2));
}
