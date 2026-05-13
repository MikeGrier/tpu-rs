// Copyright (c) 2026, Michael Grier
//! Integration tests for the new `copy`, `render`, and `setup` subcommands
//! plus the global `--on-error warn|fail` flag for tree-walking commands.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use tempfile::TempDir;

fn tpu() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tpu"));
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

fn ok(cmd: &mut Command) -> std::process::Output {
    let o = cmd.output().expect("failed to spawn tpu");
    assert!(
        o.status.success(),
        "expected exit 0 but got {}:\nstdout: {}\nstderr: {}",
        o.status,
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr),
    );
    o
}

fn write_file(p: &Path, bytes: &[u8]) {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, bytes).unwrap();
}

// ─── copy ────────────────────────────────────────────────────────────────────

#[test]
fn copy_single_file_preserves_bytes() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("a.bin");
    let dst = dir.path().join("b.bin");
    let bytes: Vec<u8> = (0u8..=255).collect();
    write_file(&src, &bytes);

    ok(tpu().arg("copy").arg(&src).arg(&dst));
    assert_eq!(fs::read(&dst).unwrap(), bytes);
}

#[test]
fn copy_into_existing_directory() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("a.txt");
    let dest_dir = dir.path().join("dest");
    fs::create_dir_all(&dest_dir).unwrap();
    write_file(&src, b"hello");

    ok(tpu().arg("copy").arg(&src).arg(&dest_dir));
    assert_eq!(fs::read(dest_dir.join("a.txt")).unwrap(), b"hello");
}

#[test]
fn copy_directory_recursive() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    write_file(&src.join("a.txt"), b"A");
    write_file(&src.join("sub").join("b.txt"), b"B");

    ok(tpu()
        .arg("copy")
        .arg("--recursive")
        .arg(&src)
        .arg(&dst));
    assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"A");
    assert_eq!(fs::read(dst.join("sub").join("b.txt")).unwrap(), b"B");
}

#[test]
fn copy_skip_existing_unless_overwrite() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("a.txt");
    let dst = dir.path().join("b.txt");
    write_file(&src, b"new");
    write_file(&dst, b"old");

    // Default: skip.
    ok(tpu().arg("copy").arg(&src).arg(&dst));
    assert_eq!(fs::read(&dst).unwrap(), b"old");

    // --overwrite replaces.
    ok(tpu().arg("copy").arg("--overwrite").arg(&src).arg(&dst));
    assert_eq!(fs::read(&dst).unwrap(), b"new");
}

// ─── render ──────────────────────────────────────────────────────────────────

#[test]
fn render_inline_template_substitutes_tokens() {
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("greeting.txt");
    ok(tpu()
        .arg("render")
        .arg(&out)
        .arg("--template")
        .arg("Hello, {{NAME}}! Today is {{DAY}}.")
        .arg("--var").arg("NAME=World")
        .arg("--var").arg("DAY=Friday"));
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        "Hello, World! Today is Friday.",
    );
}

#[test]
fn render_template_file_with_whitespace_in_braces() {
    let dir = TempDir::new().unwrap();
    let tmpl = dir.path().join("t.txt");
    let out = dir.path().join("o.txt");
    write_file(&tmpl, b"v={{ NAME }}");
    ok(tpu()
        .arg("render")
        .arg(&out)
        .arg("--template-file")
        .arg(&tmpl)
        .arg("--var").arg("NAME=ok"));
    assert_eq!(fs::read_to_string(&out).unwrap(), "v=ok");
}

#[test]
fn render_missing_token_default_errors() {
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("out.txt");
    let result = tpu()
        .arg("render")
        .arg(&out)
        .arg("--template")
        .arg("hi {{NAME}}")
        .output()
        .unwrap();
    assert!(!result.status.success(), "expected error exit");
    assert!(!out.exists(), "output file must not be written on error");
}

#[test]
fn render_missing_empty_substitutes_blank() {
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("out.txt");
    ok(tpu()
        .arg("render")
        .arg(&out)
        .arg("--template").arg("hi {{NAME}}")
        .arg("--missing").arg("empty"));
    assert_eq!(fs::read_to_string(&out).unwrap(), "hi ");
}

#[test]
fn render_missing_leave_keeps_placeholder() {
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("out.txt");
    ok(tpu()
        .arg("render")
        .arg(&out)
        .arg("--template").arg("hi {{NAME}}")
        .arg("--missing").arg("leave"));
    assert_eq!(fs::read_to_string(&out).unwrap(), "hi {{NAME}}");
}

// ─── setup ───────────────────────────────────────────────────────────────────

#[test]
fn setup_print_emits_marker_block() {
    let out = ok(tpu().arg("setup")).stdout;
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("<!-- tpu-mcp:setup:begin -->"), "begin marker missing: {s}");
    assert!(s.contains("<!-- tpu-mcp:setup:end -->"), "end marker missing");
    assert!(s.contains("tpu_copy_file"));
    assert!(s.contains("tpu_render_file"));
}

#[test]
fn setup_inject_into_fresh_file() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("sub").join("copilot-instructions.md");
    ok(tpu().arg("setup").arg("--inject").arg(&target));
    let text = fs::read_to_string(&target).unwrap();
    assert!(text.contains("<!-- tpu-mcp:setup:begin -->"));
    assert!(text.contains("<!-- tpu-mcp:setup:end -->"));
}

#[test]
fn setup_inject_is_idempotent_and_replaces_existing_block() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("ci.md");
    write_file(
        &target,
        b"# Existing\n\n<!-- tpu-mcp:setup:begin -->\nstale\n<!-- tpu-mcp:setup:end -->\nafter\n",
    );
    ok(tpu().arg("setup").arg("--inject").arg(&target));
    let after = fs::read_to_string(&target).unwrap();
    assert!(after.starts_with("# Existing"), "preamble preserved");
    assert!(after.contains("tpu_copy_file"), "block was refreshed");
    assert!(after.contains("after"), "trailing content preserved");

    // Second run is a no-op.
    let before = fs::read_to_string(&target).unwrap();
    ok(tpu().arg("setup").arg("--inject").arg(&target));
    let after = fs::read_to_string(&target).unwrap();
    assert_eq!(before, after, "second injection should be idempotent");
}

#[test]
fn setup_inject_appends_when_no_markers_present() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("notes.md");
    write_file(&target, b"original content\n");
    ok(tpu().arg("setup").arg("--inject").arg(&target));
    let after = fs::read_to_string(&target).unwrap();
    assert!(after.starts_with("original content\n"));
    assert!(after.contains("<!-- tpu-mcp:setup:begin -->"));
}

// ─── walk-error policy on `find` ─────────────────────────────────────────────

#[test]
fn find_warn_mode_continues_past_missing_path() {
    let dir = TempDir::new().unwrap();
    let real = dir.path().join("real.txt");
    write_file(&real, b"hit me");
    let missing = dir.path().join("does_not_exist");

    // With default --on-error=warn, missing path should not abort the search.
    let out = tpu()
        .arg("find")
        .arg("--pattern").arg("hit")
        .arg("--path").arg(missing.to_str().unwrap())
        .arg("--path").arg(real.to_str().unwrap())
        .output()
        .unwrap();
    assert!(out.status.success(), "warn mode should not fail");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("hit me"), "match from real file expected: {stdout}");
}

#[test]
fn find_fail_mode_aborts_on_missing_path() {
    let dir = TempDir::new().unwrap();
    let real = dir.path().join("real.txt");
    write_file(&real, b"hit me");
    let missing = dir.path().join("does_not_exist");

    let out = tpu()
        .arg("--on-error").arg("fail")
        .arg("find")
        .arg("--pattern").arg("hit")
        .arg("--path").arg(missing.to_str().unwrap())
        .arg("--path").arg(real.to_str().unwrap())
        .output()
        .unwrap();
    assert!(!out.status.success(), "fail mode must abort on missing path");
}

// Unused imports placeholder to silence warnings if `PathBuf` becomes unused.
#[allow(dead_code)]
fn _force_pathbuf_use() -> PathBuf { PathBuf::new() }
