#![allow(missing_docs)]

use std::process::Command;

const SMARTNET: &str = env!("CARGO_BIN_EXE_smartnet");

/// Runs `smartnet <args>` and returns stdout, asserting exit code 0.
#[track_caller]
fn smartnet_ok(args: &[&str]) -> String {
    let output = Command::new(SMARTNET).env("RUST_LOG", "off").args(args).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "args {args:?} failed.\nstdout: {stdout}\nstderr: {stderr}");
    stdout.into_owned()
}

#[test]
fn help() {
    let stdout = smartnet_ok(&["--help"]);
    assert!(stdout.contains("Usage"), "stdout: {stdout}");
    assert!(stdout.contains("init"), "stdout: {stdout}");
    assert!(
        !stdout.lines().any(|line| line.split_whitespace().next() == Some("node")),
        "node subcommand must not exist: {stdout}"
    );
}

#[test]
fn version() {
    let stdout = smartnet_ok(&["--version"]);
    assert!(stdout.to_lowercase().contains("smartnet"), "stdout: {stdout}");
}

#[test]
fn init_help() {
    let stdout = smartnet_ok(&["init", "--help"]);
    assert!(stdout.contains("--chain") || stdout.contains("init"), "stdout: {stdout}");
}

#[test]
fn unknown_subcommand() {
    let output = Command::new(SMARTNET).arg("definitely-not-a-cmd").output().unwrap();
    assert!(!output.status.success());
}

#[test]
fn rejects_node_subcommand() {
    let output = Command::new(SMARTNET).args(["node", "--help"]).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("node") || stderr.contains("unrecognized"), "stderr: {stderr}");
}

const fn main() {}
