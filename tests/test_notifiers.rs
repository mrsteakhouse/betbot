#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

fn betbot() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_betbot"));
    cmd.env_remove("BETBOT_DRY_RUN").env_remove("BETBOT_CONFIG");
    cmd
}

/// Local HTTP mock: accepts one connection, replies with `status` and
/// returns the received request (head and body).
fn serve(status: u16) -> (u16, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        // Bounded accept, so a bot that never connects fails the test instead of hanging it.
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut stream = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("mock server: no connection: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if let Some((k, v)) = line.split_once(':')
                && k.eq_ignore_ascii_case("content-length")
            {
                content_length = v.trim().parse().unwrap();
            }
            let end = line == "\r\n" || line.is_empty();
            request.push_str(&line);
            if end {
                break;
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();
        request.push_str(&String::from_utf8_lossy(&body));
        write!(
            stream,
            "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        request
    });
    (port, handle)
}

const TENANT: &str = "tenants:
  - name: a
    platform: { type: fake, fixture: f.yaml }
";

/// Writes `config` (with `{dir}` replaced by a temp dir holding `files`) and
/// runs betbot with `args`.
fn run(tag: &str, config: &str, files: &[(&str, &str)], args: &[&str]) -> Output {
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "betbot-test-notifiers-{tag}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, content) in files {
        std::fs::write(dir.join(name), content).unwrap();
    }
    let path = dir.join("config.yaml");
    std::fs::write(&path, config.replace("{dir}", dir.to_str().unwrap())).unwrap();
    let out = betbot()
        .arg("--config")
        .arg(&path)
        .args(args)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    out
}

fn one_notifier() -> String {
    format!("notifiers:\n  - name: hook\n    urlFile: {{dir}}/url\n{TENANT}")
}

fn output_text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn generic_url(port: u16) -> String {
    format!("generic://127.0.0.1:{port}/hook?disabletls=yes\n")
}

#[test]
fn sends_test_message_without_logging_url() {
    let (port, server) = serve(200);
    let out = run(
        "ok",
        &one_notifier(),
        &[("url", &generic_url(port))],
        &["--test-notifiers"],
    );
    let text = output_text(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    let request = server.join().unwrap();
    assert!(request.starts_with("POST /hook"), "{request}");
    assert!(request.contains("Testnachricht"), "{request}");
    assert!(request.contains("hook"), "{request}");
    assert!(text.contains("hook"), "{text}");
    assert!(!text.contains(&format!("127.0.0.1:{port}")), "{text}");
    assert!(!text.contains("/hook"), "{text}");
}

#[test]
fn missing_url_file_exits_2() {
    let out = run("missing", &one_notifier(), &[], &["--test-notifiers"]);
    assert_eq!(out.status.code(), Some(2), "{}", output_text(&out));
}

#[test]
fn invalid_url_exits_2() {
    let out = run(
        "bogus",
        &one_notifier(),
        &[("url", "bogus://x\n")],
        &["--test-notifiers"],
    );
    assert_eq!(out.status.code(), Some(2), "{}", output_text(&out));
}

#[test]
fn rejected_send_exits_1() {
    let (port, server) = serve(500);
    let out = run(
        "500",
        &one_notifier(),
        &[("url", &generic_url(port))],
        &["--test-notifiers"],
    );
    let text = output_text(&out);
    server.join().unwrap();
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("hook"), "{text}");
}

#[test]
fn no_notifiers_exits_2() {
    let out = run("none", TENANT, &[], &["--test-notifiers"]);
    assert_eq!(out.status.code(), Some(2), "{}", output_text(&out));
}

#[test]
fn conflicts_with_check_config_and_dry_run() {
    for (tag, other) in [("cc", "--check-config"), ("dr", "--dry-run")] {
        let out = run(tag, &one_notifier(), &[], &["--test-notifiers", other]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{stderr}");
        assert!(stderr.contains("cannot be used with"), "{stderr}");
    }
}

#[test]
fn build_failure_wins_but_other_notifiers_are_still_tested() {
    let (port, server) = serve(200);
    let config = format!(
        "notifiers:\n  - name: hook\n    urlFile: {{dir}}/url\n  - name: gone\n    urlFile: {{dir}}/missing\n{TENANT}"
    );
    let out = run(
        "mixed",
        &config,
        &[("url", &generic_url(port))],
        &["--test-notifiers"],
    );
    let text = output_text(&out);
    assert_eq!(out.status.code(), Some(2), "{text}");
    let request = server.join().unwrap();
    assert!(request.starts_with("POST /hook"), "{request}");
}
