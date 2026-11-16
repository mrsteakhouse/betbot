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

/// Runs `--check-config` on `config` (with `{dir}` replaced by a temp dir
/// that holds `files`) and returns the exit code.
fn check(tag: &str, config: &str, files: &[(&str, &str)]) -> Option<i32> {
    let dir =
        std::env::temp_dir().join(format!("betbot-check-config-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, content) in files {
        std::fs::write(dir.join(name), content).unwrap();
    }
    let path = dir.join("config.yaml");
    std::fs::write(&path, config.replace("{dir}", dir.to_str().unwrap())).unwrap();
    let out = betbot()
        .arg("--config")
        .arg(&path)
        .arg("--check-config")
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    out.status.code()
}

const NOTIFIER_CONFIG: &str = "notifiers:
  - name: hook
    urlFile: {dir}/url
tenants:
  - name: a
    platform: { type: fake, fixture: f.yaml }
    notifiers: [hook]
";

#[test]
fn check_config_notifiers() {
    // URL file not mounted: not checked.
    assert_eq!(check("n-missing", NOTIFIER_CONFIG, &[]), Some(0));
    assert_eq!(
        check(
            "n-valid",
            NOTIFIER_CONFIG,
            &[("url", "ntfy://ntfy.example.test/betbot\n")]
        ),
        Some(0)
    );
    assert_eq!(
        check("n-scheme", NOTIFIER_CONFIG, &[("url", "smtp://x\n")]),
        Some(2)
    );
    assert_eq!(
        check(
            "n-bad",
            NOTIFIER_CONFIG,
            &[("url", "telegram://x@telegram\n")]
        ),
        Some(2)
    );
    assert_eq!(check("n-empty", NOTIFIER_CONFIG, &[("url", "\n")]), Some(2));
    assert_eq!(
        check(
            "n-unknown",
            &NOTIFIER_CONFIG.replace("[hook]", "[other]"),
            &[]
        ),
        Some(2)
    );
}
