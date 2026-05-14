// Copyright (c) 2026, Michael Grier

//! Integration test for Milestone 4 — read-time mojibake advisory.
//!
//! Verifies the three end-to-end behaviours from the M4 theme:
//!
//! 1. Reading a clean file produces no advisory on stderr (and
//!    obviously the requested content on stdout).
//! 2. Reading a file whose decoded text contains mojibake produces
//!    exactly one `note: <path>: file appears to contain mojibake (...);
//!    run 'tpu doctor' for details` line on stderr; stdout is the
//!    file's content unchanged; exit code 0 (the read succeeds).
//! 3. The same corrupt file with the global `--no-mojibake-warning`
//!    flag is silent on stderr.
//!
//! All literal mojibake byte sequences come from
//! [`tpu::test_fixtures`] (decoded from base-64 at runtime) so this
//! source file remains pure ASCII and needs no `allow-mojibake` opt-out.

use std::{
    fs,
    process::{Command, Output, Stdio},
};

use tempfile::TempDir;
use tpu::mojibake::ALLOW_MARKER;
use tpu::test_fixtures::cafe_line;

fn tpu() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tpu"));
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

fn ok(cmd: &mut Command) -> Output {
    let o = cmd.output().expect("failed to spawn tpu");
    assert!(
        o.status.success(),
        "expected exit 0; got {}:\nstdout: {}\nstderr: {}",
        o.status,
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr),
    );
    o
}

fn write_temp(name: &str, body: &[u8]) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let p = dir.path().join(name);
    fs::write(&p, body).expect("write fixture");
    (dir, p)
}

#[test]
fn read_clean_file_emits_no_advisory() {
    let (_dir, p) = write_temp("clean.txt", b"hello world\n");
    let o = ok(tpu().arg("read").arg(&p));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "hello world\n");
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.contains("mojibake"),
        "expected no advisory; stderr was: {stderr:?}"
    );
}

#[test]
fn read_corrupt_file_emits_advisory_to_stderr_and_content_to_stdout() {
    // The canonical single-layer Latin-1 mojibake fingerprint, plus LF.
    let body = cafe_line();
    let (_dir, p) = write_temp("corrupt.txt", body.as_bytes());
    let o = ok(tpu().arg("read").arg(&p));
    // Read still returns the file's bytes verbatim.
    assert_eq!(String::from_utf8_lossy(&o.stdout), body);
    // Stderr has exactly one advisory line.
    let stderr = String::from_utf8_lossy(&o.stderr);
    let note_lines: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with("note: ") && l.contains("mojibake"))
        .collect();
    assert_eq!(
        note_lines.len(),
        1,
        "expected exactly one advisory line; stderr was: {stderr:?}"
    );
    let note = note_lines[0];
    assert!(note.contains("file appears to contain mojibake"));
    assert!(note.contains("run 'tpu doctor' for details"));
    // The path should appear in the note.
    assert!(note.contains("corrupt.txt"));
}

#[test]
fn read_corrupt_file_with_no_mojibake_warning_is_silent() {
    let body = cafe_line();
    let (_dir, p) = write_temp("corrupt.txt", body.as_bytes());
    let o = ok(tpu().arg("--no-mojibake-warning").arg("read").arg(&p));
    assert_eq!(String::from_utf8_lossy(&o.stdout), body);
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.contains("mojibake"),
        "expected silent stderr with --no-mojibake-warning; got: {stderr:?}"
    );
}

#[test]
fn read_corrupt_file_with_env_var_is_silent() {
    let body = cafe_line();
    let (_dir, p) = write_temp("corrupt.txt", body.as_bytes());
    let o = ok(tpu()
        .env("TPU_NO_MOJIBAKE_WARNING", "1")
        .arg("read")
        .arg(&p));
    assert_eq!(String::from_utf8_lossy(&o.stdout), body);
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.contains("mojibake"),
        "expected silent stderr with TPU_NO_MOJIBAKE_WARNING=1; got: {stderr:?}"
    );
}

#[test]
fn read_file_with_allow_marker_emits_no_advisory() {
    // The mojibake module's ALLOW_MARKER constant: opt-out sentinel.
    let body = format!("// {}\ncafe and {}", ALLOW_MARKER, cafe_line());
    let (_dir, p) = write_temp("opted_out.txt", body.as_bytes());
    let o = ok(tpu().arg("read").arg(&p));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.contains("mojibake"),
        "expected no advisory when allow marker is present; got: {stderr:?}"
    );
}

#[test]
fn head_corrupt_file_emits_advisory() {
    let body = format!("{}more\n", cafe_line());
    let (_dir, p) = write_temp("corrupt.txt", body.as_bytes());
    let o = ok(tpu().arg("head").arg(&p));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("mojibake"),
        "expected advisory from head; got: {stderr:?}"
    );
}

#[test]
fn tail_corrupt_file_emits_advisory() {
    let body = format!("{}more\n", cafe_line());
    let (_dir, p) = write_temp("corrupt.txt", body.as_bytes());
    let o = ok(tpu().arg("tail").arg(&p));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("mojibake"),
        "expected advisory from tail; got: {stderr:?}"
    );
}

#[test]
fn readex_corrupt_file_emits_advisory() {
    let (_dir, p) = write_temp("corrupt.txt", cafe_line().as_bytes());
    let o = ok(tpu().arg("readex").arg(&p));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("mojibake"),
        "expected advisory from readex; got: {stderr:?}"
    );
}

#[test]
fn json_mode_does_not_emit_plaintext_advisory() {
    // JSON mode: stderr is silent and stdout is NDJSON.  We must not
    // contaminate the NDJSON stream with a `note: ...` line.
    let (_dir, p) = write_temp("corrupt.txt", cafe_line().as_bytes());
    let o = ok(tpu().arg("--message-format=json").arg("read").arg(&p));
    let stdout = String::from_utf8_lossy(&o.stdout);
    // Every line of stdout must be a JSON object (NDJSON).  No bare
    // `note:` line should appear.
    for line in stdout.lines() {
        assert!(
            !line.starts_with("note:"),
            "JSON-mode stdout contains plaintext note: {stdout:?}"
        );
    }
}
