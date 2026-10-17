//! `--open`, run as Herdr runs it. A real process, because `--open` execs:
//! a failed exec inside the test runner would leave it with SIGPIPE back at
//! its default, and the next write to a closed socket would kill every test.
#![cfg(unix)]

use std::process::{Command, Stdio};

#[test]
fn missing_herdr_is_reported_not_raised() {
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-bar"))
        .arg("--open")
        .env_clear()
        .env("PATH", "/nonexistent")
        .env("HERDR_BIN_PATH", "/nonexistent/herdr")
        .stdin(Stdio::null())
        .output()
        .expect("run herdr-bar --open");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("herdr-bar: could not run herdr:"), "{}", stderr);
    assert!(output.stdout.is_empty());
}
