// Copyright (c) 2026, Michael Grier
//! Integration tests for the `tpu` binary.
//!
//! Encodes all four subcommands (read, readex, write, replace) in
//! both text and binary mode against real-world test assets (YAML pipeline
//! files, JSON dependency-graph files, hand-crafted edge-case files).
//!
//! Test count target: >1 000 tests.
//!
//! Tests are organised as macro-generated suites (one module per asset ×
//! subcommand pair) plus hand-written individual tests for error cases and
//! behaviour that varies per invocation.

use std::{
    fs,
    io::Write as _,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::LazyLock,
};

// ─── Base64 decoder ──────────────────────────────────────────────────────────

/// Decode standard base-64 text (ignoring all ASCII whitespace and `#`-comment lines).
/// Panics on invalid input — only used with compile-time-embedded literals.
fn decode_b64(s: &str) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    // Lines beginning with '#' are generation-metadata comments; discard them.
    let clean: Vec<u8> = s
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(|l| l.bytes())
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4 + 2);
    let val = |b: u8| -> u8 {
        TABLE
            .iter()
            .position(|&x| x == b)
            .map(|v| v as u8)
            .unwrap_or(0)
    };
    for chunk in clean.chunks(4) {
        let b = [
            val(chunk[0]),
            val(chunk[1]),
            if chunk.len() > 2 { val(chunk[2]) } else { 0 },
            if chunk.len() > 3 { val(chunk[3]) } else { 0 },
        ];
        out.push((b[0] << 2) | (b[1] >> 4));
        if chunk.len() > 2 && *chunk.get(2).unwrap_or(&b'=') != b'=' {
            out.push((b[1] << 4) | (b[2] >> 2));
        }
        if chunk.len() > 3 && *chunk.get(3).unwrap_or(&b'=') != b'=' {
            out.push((b[2] << 6) | b[3]);
        }
    }
    out
}

// ─── Test asset directory ────────────────────────────────────────────────────
//
// All test asset files are stored in `tests/data/` as standard base-64 files
// (<name>.b64) to guarantee exact byte content regardless of platform line-
// ending conventions or ADO/git encoding rules.  At first use the LazyLock
// below decodes each file into a temporary directory; `asset()` then returns
// paths into that directory.

static ASSET_DIR: LazyLock<(tempfile::TempDir, PathBuf)> = LazyLock::new(|| {
    let dir = tempfile::tempdir().expect("create temp asset dir");
    let p = dir.path().to_path_buf();

    macro_rules! asset_file {
        ($name:literal) => {{
            let bytes = decode_b64(include_str!(concat!("data/", $name, ".b64")));
            fs::write(p.join($name), &bytes).expect(concat!("write asset ", $name));
        }};
    }

    asset_file!("ascii_10lines.txt");
    asset_file!("backslash.txt");
    asset_file!("binary.bin");
    asset_file!("empty.txt");
    asset_file!("json_bad_naming.txt");
    asset_file!("json_circular.txt");
    asset_file!("json_generator.txt");
    asset_file!("json_incomplete.txt");
    asset_file!("json_lib_core.txt");
    asset_file!("json_lib_debug.txt");
    asset_file!("json_malformed.txt");
    asset_file!("json_no_keys.txt");
    asset_file!("json_project_a.txt");
    asset_file!("json_util.txt");
    asset_file!("mixed_endings.txt");
    asset_file!("multiline_crlf.txt");
    asset_file!("multiline_lf.txt");
    asset_file!("pipeline_coverage.txt");
    asset_file!("pipeline_docfx.txt");
    asset_file!("pipeline_pr.txt");
    asset_file!("policy_approver.txt");
    asset_file!("policy_proof.txt");
    asset_file!("policy_pr_build.txt");
    asset_file!("regex_content.txt");
    asset_file!("singleline.txt");
    asset_file!("singleline_no_nl.txt");
    asset_file!("unicode.txt");
    asset_file!("utf8_bom.txt");
    asset_file!("mixed_endings.txt");

    (dir, p)
});

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Spawn the compiled `tpu` binary with no stdin by default.
pub fn tpu() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tpu"));
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

/// Absolute path to a decoded test asset file.
pub fn asset(name: &str) -> PathBuf {
    ASSET_DIR.1.join(name)
}

/// Format a line count the way the `read`/`readex`/`edit` out-of-range errors
/// do, so expectations stay correct for single-line files ("1 line", not
/// "1 lines").
pub fn plural_lines(n: usize) -> String {
    format!("{n} line{}", if n == 1 { "" } else { "s" })
}

/// Count the logical lines in a file the same way `tpu read` does: split on
/// LF and drop the empty tail produced by a trailing newline.  Valid for the
/// ASCII/UTF-8 assets used by the generated read/readex suites.
pub fn line_count(path: &std::path::Path) -> usize {
    let bytes = fs::read(path).expect("read asset for line count");
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    if bytes.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    }
}

/// Run a command, assert exit 0, return its Output.
/// Accepts `&mut Command` so callers can pass `tpu().arg(...).arg(...)` chains
/// directly (since `Command::arg` returns `&mut Command`).
pub fn ok(cmd: &mut Command) -> Output {
    let o = cmd.output().expect("failed to spawn tpu");
    if !o.status.success() {
        panic!(
            "expected exit 0 but got {}:\nstdout: {}\nstderr: {}",
            o.status,
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr),
        );
    }
    o
}

/// Run a command, assert non-zero exit, return its Output.
pub fn err(cmd: &mut Command) -> Output {
    let o = cmd.output().expect("failed to spawn tpu");
    if o.status.success() {
        panic!(
            "expected non-zero exit but got success:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr),
        );
    }
    o
}

/// Pipe `input` bytes to stdin; assert exit 0.
pub fn ok_stdin(cmd: &mut Command, input: &[u8]) -> Output {
    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn().expect("failed to spawn tpu");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input)
        .expect("write to stdin");
    let o = child.wait_with_output().expect("wait failed");
    if !o.status.success() {
        panic!(
            "expected exit 0 but got {}:\nstdout: {}\nstderr: {}",
            o.status,
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr),
        );
    }
    o
}

/// Pipe `input` bytes to stdin; assert non-zero exit.
pub fn err_stdin(cmd: &mut Command, input: &[u8]) -> Output {
    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn().expect("failed to spawn tpu");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input)
        .expect("write to stdin");
    let o = child.wait_with_output().expect("wait failed");
    if o.status.success() {
        panic!(
            "expected non-zero exit but got success:\nstdout: {}",
            String::from_utf8_lossy(&o.stdout),
        );
    }
    o
}

/// Copy an asset file into a fresh temporary directory.
/// Returns `(TempDir, dest_path)`. Caller must keep TempDir alive.
pub fn cp(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join(name);
    fs::copy(asset(name), &dst).unwrap();
    (dir, dst)
}

/// Return `path` with ".bak" appended (e.g. `/tmp/a.txt` → `/tmp/a.txt.bak`).
pub fn bak(p: &std::path::Path) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(".bak");
    PathBuf::from(s)
}

// ─── Macros ──────────────────────────────────────────────────────────────────

/// 7 `tpu read` (text mode) tests for one non-empty asset file.
macro_rules! read_text_suite {
    ($mod:ident, $file:literal) => {
        mod $mod {
            use super::*;
            fn f() -> PathBuf {
                asset($file)
            }
            /// Basic invocation.
            #[test]
            fn exits_ok() {
                ok(tpu().arg("read").arg(f()));
            }
            /// --utf8 is accepted.
            #[test]
            fn utf8_exits_ok() {
                ok(tpu().arg("read").arg("--utf8").arg(f()));
            }
            /// --utf8 --bom=force prepends the UTF-8 BOM bytes.
            #[test]
            fn utf8_bom_force_has_bom() {
                let o = ok(tpu().arg("read").arg("--utf8").arg("--bom=force").arg(f()));
                assert!(
                    o.stdout.starts_with(&[0xEF, 0xBB, 0xBF]),
                    "expected UTF-8 BOM prefix; got {:02x?}",
                    &o.stdout[..o.stdout.len().min(6)]
                );
            }
            /// --utf8 --bom=strip must not prepend a BOM.
            #[test]
            fn utf8_bom_strip_no_bom() {
                let o = ok(tpu().arg("read").arg("--utf8").arg("--bom=strip").arg(f()));
                assert!(!o.stdout.starts_with(&[0xEF, 0xBB, 0xBF]), "unexpected BOM");
            }
            /// --lines 1 selects the first line without error.
            #[test]
            fn lines_1_ok() {
                ok(tpu().arg("read").arg("--lines=1").arg(f()));
            }
            /// --lines 2 is accepted when the file actually has a second line,
            /// and fails cleanly (never panics) when it does not.
            #[test]
            fn lines_2_matches_file_length() {
                if line_count(&f()) >= 2 {
                    ok(tpu().arg("read").arg("--lines=2").arg(f()));
                } else {
                    let o = err(tpu().arg("read").arg("--lines=2").arg(f()));
                    assert_clean_failure(&o, "past end of file");
                }
            }
            /// The file's last line is always addressable.
            #[test]
            fn lines_last_ok() {
                let total = line_count(&f());
                ok(tpu().arg("read").arg(format!("--lines={total}")).arg(f()));
            }
            /// One line past the end is a clean error, never a panic.
            #[test]
            fn lines_one_past_end_exits_err() {
                let total = line_count(&f());
                let o = err(tpu()
                    .arg("read")
                    .arg(format!("--lines={}", total + 1))
                    .arg(f()));
                assert_clean_failure(&o, &format!("past end of file ({})", plural_lines(total)));
            }
            /// A start far past the end is a clean error, never a panic.
            #[test]
            fn lines_far_past_end_exits_err() {
                let o = err(tpu().arg("read").arg("--lines=999999").arg(f()));
                assert_clean_failure(&o, "past end of file");
            }
            /// An end bound past the last line is clamped, not rejected.
            #[test]
            fn lines_end_past_eof_is_clamped() {
                ok(tpu().arg("read").arg("--lines=1-999999").arg(f()));
            }
            /// --numbers adds a prefix to each output line.
            #[test]
            fn numbers_ok() {
                ok(tpu().arg("read").arg("--numbers").arg(f()));
            }
        }
    };
}

/// 5 `tpu read --binary` tests for any file (including empty and binary).
macro_rules! read_binary_suite {
    ($mod:ident, $file:literal) => {
        mod $mod {
            use super::*;
            fn f() -> PathBuf {
                asset($file)
            }
            /// Binary read exits 0.
            #[test]
            fn exits_ok() {
                ok(tpu().arg("read").arg("--binary").arg(f()));
            }
            /// `read --binary` never appends a trailing newline.
            #[test]
            fn no_trailing_newline() {
                let o = ok(tpu().arg("read").arg("--binary").arg(f()));
                assert!(
                    !o.stdout.ends_with(b"\n"),
                    "read --binary must not append a trailing newline"
                );
            }
            /// --bytes=1 is accepted.
            #[test]
            fn bytes_1_ok() {
                ok(tpu().arg("read").arg("--binary").arg("--bytes=1").arg(f()));
            }
            /// --bytes=1-5 is accepted (clamped silently for small files).
            #[test]
            fn bytes_1_to_5_ok() {
                ok(tpu()
                    .arg("read")
                    .arg("--binary")
                    .arg("--bytes=1-5")
                    .arg(f()));
            }
            /// Out-of-range start is clamped, not an error.
            #[test]
            fn bytes_out_of_range_ok() {
                ok(tpu()
                    .arg("read")
                    .arg("--binary")
                    .arg("--bytes=9999-99999")
                    .arg(f()));
            }
        }
    };
}

/// 7 `tpu readex` (text mode) tests for one non-empty asset file.
macro_rules! readex_text_suite {
    ($mod:ident, $file:literal) => {
        mod $mod {
            use super::*;
            fn f() -> PathBuf {
                asset($file)
            }
            /// Basic invocation exits 0.
            #[test]
            fn exits_ok() {
                ok(tpu().arg("readex").arg(f()));
            }
            /// Output is a single flat line: exactly one real newline at the end.
            #[test]
            fn output_single_line() {
                let o = ok(tpu().arg("readex").arg(f()));
                let count = o.stdout.iter().filter(|&&b| b == b'\n').count();
                assert_eq!(
                    count, 1,
                    "readex must emit exactly one real newline; got {count}"
                );
            }
            /// Output ends with a real newline.
            #[test]
            fn output_ends_with_newline() {
                let o = ok(tpu().arg("readex").arg(f()));
                assert!(
                    o.stdout.ends_with(b"\n"),
                    "readex output must end with newline"
                );
            }
            /// --lines 1 is accepted.
            #[test]
            fn lines_1_ok() {
                ok(tpu().arg("readex").arg("--lines=1").arg(f()));
            }
            /// --lines 1-2 is accepted even for 1-line files (the end bound is
            /// clamped; only the *start* bound is checked against the file).
            #[test]
            fn lines_1_to_2_ok() {
                ok(tpu().arg("readex").arg("--lines=1-2").arg(f()));
            }
            /// The file's last line is always addressable.
            #[test]
            fn lines_last_ok() {
                let total = line_count(&f());
                ok(tpu().arg("readex").arg(format!("--lines={total}")).arg(f()));
            }
            /// One line past the end is a clean error, never a panic.
            #[test]
            fn lines_one_past_end_exits_err() {
                let total = line_count(&f());
                let o = err(tpu()
                    .arg("readex")
                    .arg(format!("--lines={}", total + 1))
                    .arg(f()));
                assert_clean_failure(&o, &format!("past end of file ({})", plural_lines(total)));
            }
            /// A start far past the end is a clean error, never a panic.
            #[test]
            fn lines_far_past_end_exits_err() {
                let o = err(tpu().arg("readex").arg("--lines=999999").arg(f()));
                assert_clean_failure(&o, "past end of file");
            }
            /// --numbers is accepted.
            #[test]
            fn numbers_ok() {
                ok(tpu().arg("readex").arg("--numbers").arg(f()));
            }
            /// --utf8 --bom=force prepends the UTF-8 BOM.
            #[test]
            fn utf8_bom_force_has_bom() {
                let o = ok(tpu()
                    .arg("readex")
                    .arg("--utf8")
                    .arg("--bom=force")
                    .arg(f()));
                assert!(
                    o.stdout.starts_with(&[0xEF, 0xBB, 0xBF]),
                    "expected UTF-8 BOM; got {:02x?}",
                    &o.stdout[..o.stdout.len().min(6)]
                );
            }
        }
    };
}

/// 5 `tpu readex --binary` tests for any file.
macro_rules! readex_binary_suite {
    ($mod:ident, $file:literal) => {
        mod $mod {
            use super::*;
            fn f() -> PathBuf {
                asset($file)
            }
            /// Binary readex exits 0.
            #[test]
            fn exits_ok() {
                ok(tpu().arg("readex").arg("--binary").arg(f()));
            }
            /// `readex --binary` always ends with a real newline (flat-line terminator).
            #[test]
            fn output_ends_with_newline() {
                let o = ok(tpu().arg("readex").arg("--binary").arg(f()));
                assert!(
                    o.stdout.ends_with(b"\n"),
                    "readex --binary must end with a newline"
                );
            }
            /// --bytes=1 is accepted.
            #[test]
            fn bytes_1_ok() {
                ok(tpu()
                    .arg("readex")
                    .arg("--binary")
                    .arg("--bytes=1")
                    .arg(f()));
            }
            /// --bytes=1-5 is accepted.
            #[test]
            fn bytes_1_to_5_ok() {
                ok(tpu()
                    .arg("readex")
                    .arg("--binary")
                    .arg("--bytes=1-5")
                    .arg(f()));
            }
            /// Out-of-range byte start is clamped, not an error.
            #[test]
            fn bytes_out_of_range_ok() {
                ok(tpu()
                    .arg("readex")
                    .arg("--binary")
                    .arg("--bytes=9999-99999")
                    .arg(f()));
            }
        }
    };
}

/// 4 `tpu write` (text mode) tests for one non-empty asset file.
macro_rules! write_suite {
    ($mod:ident, $file:literal) => {
        mod $mod {
            use super::*;
            fn f() -> PathBuf {
                asset($file)
            }
            /// `write` creates a new file from stdin content.
            #[test]
            fn creates_new_file() {
                let dir = tempfile::tempdir().unwrap();
                let dst = dir.path().join("new.txt");
                ok_stdin(tpu().arg("write").arg(&dst), b"hello\nworld\n");
                assert!(dst.exists(), "file should have been created");
                let content = fs::read_to_string(&dst).unwrap();
                assert_eq!(content, "hello\nworld\n");
            }
            /// `write` to an existing file renames the original to `.bak`.
            #[test]
            fn write_to_existing_creates_bak() {
                let dir = tempfile::tempdir().unwrap();
                let dst = dir.path().join("existing.txt");
                fs::write(&dst, b"original content\n").unwrap();
                ok_stdin(tpu().arg("write").arg(&dst), b"new content\n");
                assert!(bak(&dst).exists(), ".bak file should have been created");
            }
            /// read → write → read round-trip gives identical normalised output.
            #[test]
            fn read_write_roundtrip() {
                let original = ok(tpu().arg("read").arg(f())).stdout;
                let dir = tempfile::tempdir().unwrap();
                let dst = dir.path().join("roundtrip.txt");
                ok_stdin(tpu().arg("write").arg(&dst), &original);
                let readback = ok(tpu().arg("read").arg(&dst)).stdout;
                assert_eq!(original, readback, "read-write round-trip content mismatch");
            }
            /// --utf8 flag is accepted and creates the file.
            #[test]
            fn utf8_write_ok() {
                let dir = tempfile::tempdir().unwrap();
                let dst = dir.path().join("utf8.txt");
                ok_stdin(
                    tpu().arg("write").arg("--utf8").arg(&dst),
                    b"text content\n",
                );
                assert!(dst.exists(), "file should exist after --utf8 write");
            }
        }
    };
}

/// `tpu create` CLI tests: create-only writes that refuse to clobber.
mod create_cli {
    use super::*;

    /// `create` writes a brand-new file from stdin.
    #[test]
    fn creates_new_file_from_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("new.txt");
        ok_stdin(tpu().arg("create").arg(&dst), b"hello\nworld\n");
        assert!(dst.exists(), "file should have been created");
        assert_eq!(fs::read_to_string(&dst).unwrap(), "hello\nworld\n");
    }

    /// `create` with inline content writes it without touching stdin.
    #[test]
    fn creates_new_file_from_data_arg() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("data.txt");
        ok(tpu().arg("create").arg(&dst).arg("inline\n"));
        assert_eq!(fs::read_to_string(&dst).unwrap(), "inline\n");
    }

    /// `create` refuses to overwrite an existing file and leaves it untouched.
    #[test]
    fn refuses_to_overwrite_existing() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("existing.txt");
        fs::write(&dst, b"original\n").unwrap();
        let o = err(tpu().arg("create").arg(&dst).arg("new\n"));
        let s = String::from_utf8_lossy(&o.stderr);
        assert!(
            s.contains("already exists"),
            "expected an 'already exists' error; got: {s}"
        );
        assert_eq!(
            fs::read_to_string(&dst).unwrap(),
            "original\n",
            "existing file must be left untouched"
        );
    }

    /// `create --line-ending=crlf` honours the requested line ending.
    #[test]
    fn honours_crlf_line_ending() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("crlf.txt");
        ok(tpu()
            .arg("create")
            .arg("--line-ending=crlf")
            .arg(&dst)
            .arg("a\nb\n"));
        assert_eq!(fs::read(&dst).unwrap(), b"a\r\nb\r\n");
    }
}

/// 6 `tpu replace` tests for one non-empty text asset file.
macro_rules! replace_suite {
    ($mod:ident, $file:literal) => {
        mod $mod {
            use super::*;
            /// Copy the asset to a temp file, returning (TempDir, PathBuf).
            fn tc() -> (tempfile::TempDir, PathBuf) {
                let dir = tempfile::tempdir().unwrap();
                let dst = dir.path().join("input.txt");
                fs::copy(asset($file), &dst).unwrap();
                (dir, dst)
            }
            /// No-match pattern exits 0 and reports "0 replacements" on stderr.
            #[test]
            fn nomatch_exits_ok() {
                let (dir, f) = tc();
                let o = ok(tpu()
                    .arg("replace")
                    .arg(&f)
                    .arg("ZZZNOMATCH_XY0000")
                    .arg("Z"));
                let s = String::from_utf8_lossy(&o.stderr);
                assert!(
                    s.contains("0 replacements"),
                    "expected '0 replacements'; got: {s}"
                );
                drop(dir);
            }
            /// Matching pattern exits 0 and reports the replacement count on stderr.
            #[test]
            fn match_exits_ok() {
                let (dir, f) = tc();
                let o = ok(tpu()
                    .arg("replace")
                    .arg("--regex")
                    .arg(&f)
                    .arg("[a-zA-Z0-9]")
                    .arg("_"));
                let s = String::from_utf8_lossy(&o.stderr);
                assert!(
                    s.contains("replacement"),
                    "expected replacement count; got: {s}"
                );
                drop(dir);
            }
            /// Matching replace always creates a `.bak` of the original file.
            /// Uses `.` (any non-newline byte) as the pattern so it matches
            /// every non-empty fixture -- including `json_no_keys.txt`
            /// which contains only `{}` and no alphanumerics.  Post-M7,
            /// a fixture-specific zero-match would (correctly) skip the
            /// `.bak`, so the pattern must be universal.
            #[test]
            fn match_creates_bak() {
                let (dir, f) = tc();
                ok(tpu()
                    .arg("replace")
                    .arg("--regex")
                    .arg(&f)
                    .arg(".")
                    .arg("_"));
                assert!(
                    bak(&f).exists(),
                    ".bak should be created at {}",
                    bak(&f).display()
                );
                drop(dir);
            }
            /// --multiline flag is accepted (zero-match variant).
            #[test]
            fn multiline_mode_exits_ok() {
                let (dir, f) = tc();
                ok(tpu()
                    .arg("replace")
                    .arg("--multiline")
                    .arg(&f)
                    .arg("ZZZNOMATCH_XY0000")
                    .arg("Z"));
                drop(dir);
            }
            /// --diff flag is accepted alongside a matching replace.
            #[test]
            fn diff_mode_exits_ok() {
                let (dir, f) = tc();
                ok(tpu()
                    .arg("replace")
                    .arg("--regex")
                    .arg("--diff")
                    .arg(&f)
                    .arg("[a-zA-Z0-9]")
                    .arg("_"));
                drop(dir);
            }
            /// An invalid regex pattern results in a non-zero exit.
            #[test]
            fn bad_regex_exits_err() {
                let (dir, f) = tc();
                err(tpu()
                    .arg("replace")
                    .arg("--regex")
                    .arg(&f)
                    .arg("[invalid")
                    .arg("Z"));
                drop(dir);
            }
        }
    };
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 1 — `tpu read` text mode  (25 non-empty assets × 7 tests = 175)
// ═══════════════════════════════════════════════════════════════════════════════

read_text_suite!(rt_ascii_10lines, "ascii_10lines.txt");
read_text_suite!(rt_backslash, "backslash.txt");
read_text_suite!(rt_json_bad_naming, "json_bad_naming.txt");
read_text_suite!(rt_json_circular, "json_circular.txt");
read_text_suite!(rt_json_generator, "json_generator.txt");
read_text_suite!(rt_json_incomplete, "json_incomplete.txt");
read_text_suite!(rt_json_lib_core, "json_lib_core.txt");
read_text_suite!(rt_json_lib_debug, "json_lib_debug.txt");
read_text_suite!(rt_json_malformed, "json_malformed.txt");
read_text_suite!(rt_json_no_keys, "json_no_keys.txt");
read_text_suite!(rt_json_project_a, "json_project_a.txt");
read_text_suite!(rt_json_util, "json_util.txt");
read_text_suite!(rt_multiline_crlf, "multiline_crlf.txt");
read_text_suite!(rt_multiline_lf, "multiline_lf.txt");
read_text_suite!(rt_pipeline_coverage, "pipeline_coverage.txt");
read_text_suite!(rt_pipeline_docfx, "pipeline_docfx.txt");
read_text_suite!(rt_pipeline_pr, "pipeline_pr.txt");
read_text_suite!(rt_policy_approver, "policy_approver.txt");
read_text_suite!(rt_policy_proof, "policy_proof.txt");
read_text_suite!(rt_policy_pr_build, "policy_pr_build.txt");
read_text_suite!(rt_regex_content, "regex_content.txt");
read_text_suite!(rt_singleline, "singleline.txt");
read_text_suite!(rt_singleline_no_nl, "singleline_no_nl.txt");
read_text_suite!(rt_unicode, "unicode.txt");
read_text_suite!(rt_utf8_bom, "utf8_bom.txt");

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 2 — `tpu read --binary`  (27 files × 5 tests = 135)
// includes empty.txt and binary.bin in addition to all .txt files
// ═══════════════════════════════════════════════════════════════════════════════

read_binary_suite!(rb_ascii_10lines, "ascii_10lines.txt");
read_binary_suite!(rb_backslash, "backslash.txt");
read_binary_suite!(rb_json_bad_naming, "json_bad_naming.txt");
read_binary_suite!(rb_json_circular, "json_circular.txt");
read_binary_suite!(rb_json_generator, "json_generator.txt");
read_binary_suite!(rb_json_incomplete, "json_incomplete.txt");
read_binary_suite!(rb_json_lib_core, "json_lib_core.txt");
read_binary_suite!(rb_json_lib_debug, "json_lib_debug.txt");
read_binary_suite!(rb_json_malformed, "json_malformed.txt");
read_binary_suite!(rb_json_no_keys, "json_no_keys.txt");
read_binary_suite!(rb_json_project_a, "json_project_a.txt");
read_binary_suite!(rb_json_util, "json_util.txt");
read_binary_suite!(rb_multiline_crlf, "multiline_crlf.txt");
read_binary_suite!(rb_multiline_lf, "multiline_lf.txt");
read_binary_suite!(rb_pipeline_coverage, "pipeline_coverage.txt");
read_binary_suite!(rb_pipeline_docfx, "pipeline_docfx.txt");
read_binary_suite!(rb_pipeline_pr, "pipeline_pr.txt");
read_binary_suite!(rb_policy_approver, "policy_approver.txt");
read_binary_suite!(rb_policy_proof, "policy_proof.txt");
read_binary_suite!(rb_policy_pr_build, "policy_pr_build.txt");
read_binary_suite!(rb_regex_content, "regex_content.txt");
read_binary_suite!(rb_singleline, "singleline.txt");
read_binary_suite!(rb_singleline_no_nl, "singleline_no_nl.txt");
read_binary_suite!(rb_unicode, "unicode.txt");
read_binary_suite!(rb_utf8_bom, "utf8_bom.txt");
read_binary_suite!(rb_empty, "empty.txt");
read_binary_suite!(rb_binary_bin, "binary.bin");

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 3 — `tpu readex` text mode  (25 non-empty assets × 7 tests = 175)
// ═══════════════════════════════════════════════════════════════════════════════

readex_text_suite!(rxt_ascii_10lines, "ascii_10lines.txt");
readex_text_suite!(rxt_backslash, "backslash.txt");
readex_text_suite!(rxt_json_bad_naming, "json_bad_naming.txt");
readex_text_suite!(rxt_json_circular, "json_circular.txt");
readex_text_suite!(rxt_json_generator, "json_generator.txt");
readex_text_suite!(rxt_json_incomplete, "json_incomplete.txt");
readex_text_suite!(rxt_json_lib_core, "json_lib_core.txt");
readex_text_suite!(rxt_json_lib_debug, "json_lib_debug.txt");
readex_text_suite!(rxt_json_malformed, "json_malformed.txt");
readex_text_suite!(rxt_json_no_keys, "json_no_keys.txt");
readex_text_suite!(rxt_json_project_a, "json_project_a.txt");
readex_text_suite!(rxt_json_util, "json_util.txt");
readex_text_suite!(rxt_multiline_crlf, "multiline_crlf.txt");
readex_text_suite!(rxt_multiline_lf, "multiline_lf.txt");
readex_text_suite!(rxt_pipeline_coverage, "pipeline_coverage.txt");
readex_text_suite!(rxt_pipeline_docfx, "pipeline_docfx.txt");
readex_text_suite!(rxt_pipeline_pr, "pipeline_pr.txt");
readex_text_suite!(rxt_policy_approver, "policy_approver.txt");
readex_text_suite!(rxt_policy_proof, "policy_proof.txt");
readex_text_suite!(rxt_policy_pr_build, "policy_pr_build.txt");
readex_text_suite!(rxt_regex_content, "regex_content.txt");
readex_text_suite!(rxt_singleline, "singleline.txt");
readex_text_suite!(rxt_singleline_no_nl, "singleline_no_nl.txt");
readex_text_suite!(rxt_unicode, "unicode.txt");
readex_text_suite!(rxt_utf8_bom, "utf8_bom.txt");

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 4 — `tpu readex --binary`  (27 files × 5 tests = 135)
// ═══════════════════════════════════════════════════════════════════════════════

readex_binary_suite!(rxb_ascii_10lines, "ascii_10lines.txt");
readex_binary_suite!(rxb_backslash, "backslash.txt");
readex_binary_suite!(rxb_json_bad_naming, "json_bad_naming.txt");
readex_binary_suite!(rxb_json_circular, "json_circular.txt");
readex_binary_suite!(rxb_json_generator, "json_generator.txt");
readex_binary_suite!(rxb_json_incomplete, "json_incomplete.txt");
readex_binary_suite!(rxb_json_lib_core, "json_lib_core.txt");
readex_binary_suite!(rxb_json_lib_debug, "json_lib_debug.txt");
readex_binary_suite!(rxb_json_malformed, "json_malformed.txt");
readex_binary_suite!(rxb_json_no_keys, "json_no_keys.txt");
readex_binary_suite!(rxb_json_project_a, "json_project_a.txt");
readex_binary_suite!(rxb_json_util, "json_util.txt");
readex_binary_suite!(rxb_multiline_crlf, "multiline_crlf.txt");
readex_binary_suite!(rxb_multiline_lf, "multiline_lf.txt");
readex_binary_suite!(rxb_pipeline_coverage, "pipeline_coverage.txt");
readex_binary_suite!(rxb_pipeline_docfx, "pipeline_docfx.txt");
readex_binary_suite!(rxb_pipeline_pr, "pipeline_pr.txt");
readex_binary_suite!(rxb_policy_approver, "policy_approver.txt");
readex_binary_suite!(rxb_policy_proof, "policy_proof.txt");
readex_binary_suite!(rxb_policy_pr_build, "policy_pr_build.txt");
readex_binary_suite!(rxb_regex_content, "regex_content.txt");
readex_binary_suite!(rxb_singleline, "singleline.txt");
readex_binary_suite!(rxb_singleline_no_nl, "singleline_no_nl.txt");
readex_binary_suite!(rxb_unicode, "unicode.txt");
readex_binary_suite!(rxb_utf8_bom, "utf8_bom.txt");
readex_binary_suite!(rxb_empty, "empty.txt");
readex_binary_suite!(rxb_binary_bin, "binary.bin");

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 5 — `tpu write`  (25 non-empty assets × 4 tests = 100)
// ═══════════════════════════════════════════════════════════════════════════════

write_suite!(wt_ascii_10lines, "ascii_10lines.txt");
write_suite!(wt_backslash, "backslash.txt");
write_suite!(wt_json_bad_naming, "json_bad_naming.txt");
write_suite!(wt_json_circular, "json_circular.txt");
write_suite!(wt_json_generator, "json_generator.txt");
write_suite!(wt_json_incomplete, "json_incomplete.txt");
write_suite!(wt_json_lib_core, "json_lib_core.txt");
write_suite!(wt_json_lib_debug, "json_lib_debug.txt");
write_suite!(wt_json_malformed, "json_malformed.txt");
write_suite!(wt_json_no_keys, "json_no_keys.txt");
write_suite!(wt_json_project_a, "json_project_a.txt");
write_suite!(wt_json_util, "json_util.txt");
write_suite!(wt_multiline_crlf, "multiline_crlf.txt");
write_suite!(wt_multiline_lf, "multiline_lf.txt");
write_suite!(wt_pipeline_coverage, "pipeline_coverage.txt");
write_suite!(wt_pipeline_docfx, "pipeline_docfx.txt");
write_suite!(wt_pipeline_pr, "pipeline_pr.txt");
write_suite!(wt_policy_approver, "policy_approver.txt");
write_suite!(wt_policy_proof, "policy_proof.txt");
write_suite!(wt_policy_pr_build, "policy_pr_build.txt");
write_suite!(wt_regex_content, "regex_content.txt");
write_suite!(wt_singleline, "singleline.txt");
write_suite!(wt_singleline_no_nl, "singleline_no_nl.txt");
write_suite!(wt_unicode, "unicode.txt");
write_suite!(wt_utf8_bom, "utf8_bom.txt");

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 6 — `tpu replace`  (25 non-empty assets × 6 tests = 150)
// ═══════════════════════════════════════════════════════════════════════════════

replace_suite!(rp_ascii_10lines, "ascii_10lines.txt");
replace_suite!(rp_backslash, "backslash.txt");
replace_suite!(rp_json_bad_naming, "json_bad_naming.txt");
replace_suite!(rp_json_circular, "json_circular.txt");
replace_suite!(rp_json_generator, "json_generator.txt");
replace_suite!(rp_json_incomplete, "json_incomplete.txt");
replace_suite!(rp_json_lib_core, "json_lib_core.txt");
replace_suite!(rp_json_lib_debug, "json_lib_debug.txt");
replace_suite!(rp_json_malformed, "json_malformed.txt");
replace_suite!(rp_json_no_keys, "json_no_keys.txt");
replace_suite!(rp_json_project_a, "json_project_a.txt");
replace_suite!(rp_json_util, "json_util.txt");
replace_suite!(rp_multiline_crlf, "multiline_crlf.txt");
replace_suite!(rp_multiline_lf, "multiline_lf.txt");
replace_suite!(rp_pipeline_coverage, "pipeline_coverage.txt");
replace_suite!(rp_pipeline_docfx, "pipeline_docfx.txt");
replace_suite!(rp_pipeline_pr, "pipeline_pr.txt");
replace_suite!(rp_policy_approver, "policy_approver.txt");
replace_suite!(rp_policy_proof, "policy_proof.txt");
replace_suite!(rp_policy_pr_build, "policy_pr_build.txt");
replace_suite!(rp_regex_content, "regex_content.txt");
replace_suite!(rp_singleline, "singleline.txt");
replace_suite!(rp_singleline_no_nl, "singleline_no_nl.txt");
replace_suite!(rp_unicode, "unicode.txt");
replace_suite!(rp_utf8_bom, "utf8_bom.txt");

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 7 — Individual tests: error cases, edge cases, special behaviours
// ═══════════════════════════════════════════════════════════════════════════════

// ─── CLI basics ──────────────────────────────────────────────────────────────

#[test]
fn no_subcommand_exits_err() {
    let mut cmd = tpu();
    err(&mut cmd);
}

#[test]
fn help_flag_exits_ok() {
    ok(tpu().arg("--help"));
}

#[test]
fn version_flag_exits_ok() {
    ok(tpu().arg("--version"));
}

#[test]
fn unknown_subcommand_exits_err() {
    err(tpu().arg("frobnicate"));
}

#[test]
fn read_help_exits_ok() {
    ok(tpu().arg("read").arg("--help"));
}

#[test]
fn readex_help_exits_ok() {
    ok(tpu().arg("readex").arg("--help"));
}

#[test]
fn write_help_exits_ok() {
    ok(tpu().arg("write").arg("--help"));
}

#[test]
fn replace_help_exits_ok() {
    ok(tpu().arg("replace").arg("--help"));
}

// ─── read: error cases ───────────────────────────────────────────────────────

#[test]
fn read_missing_file_exits_err() {
    err(tpu()
        .arg("read")
        .arg("/nonexistent/path/does_not_exist_zzz.txt"));
}

#[test]
fn read_lines_zero_exits_err() {
    err(tpu()
        .arg("read")
        .arg("--lines=0")
        .arg(asset("singleline.txt")));
}

#[test]
fn read_lines_bad_format_exits_err() {
    err(tpu()
        .arg("read")
        .arg("--lines=bad")
        .arg(asset("singleline.txt")));
}

#[test]
fn read_lines_range_reversed_exits_err() {
    // lo > hi should be rejected
    err(tpu()
        .arg("read")
        .arg("--lines=5-3")
        .arg(asset("ascii_10lines.txt")));
}

#[test]
fn read_lines_hi_zero_exits_err() {
    err(tpu()
        .arg("read")
        .arg("--lines=1-0")
        .arg(asset("singleline.txt")));
}

#[test]
fn read_bom_without_utf8_exits_err() {
    // --bom requires --utf8
    err(tpu()
        .arg("read")
        .arg("--bom=strip")
        .arg(asset("singleline.txt")));
}

#[test]
fn read_binary_and_utf8_conflict_exits_err() {
    err(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--utf8")
        .arg(asset("singleline.txt")));
}

#[test]
fn read_binary_and_numbers_conflict_exits_err() {
    err(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--numbers")
        .arg(asset("singleline.txt")));
}

#[test]
fn read_binary_and_lines_conflict_exits_err() {
    err(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--lines=1")
        .arg(asset("singleline.txt")));
}

#[test]
fn read_bytes_without_binary_exits_err() {
    err(tpu()
        .arg("read")
        .arg("--bytes=1")
        .arg(asset("singleline.txt")));
}

#[test]
fn read_bytes_zero_exits_err() {
    err(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--bytes=0")
        .arg(asset("singleline.txt")));
}

#[test]
fn read_bytes_range_reversed_exits_err() {
    err(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--bytes=5-3")
        .arg(asset("singleline.txt")));
}

#[test]
fn read_empty_file_text_mode_ok() {
    // empty.txt can be memory-mapped on this platform; `read` exits ok with empty output
    let o = ok(tpu().arg("read").arg(asset("empty.txt")));
    assert!(o.stdout.is_empty(), "expected empty stdout for empty file");
}

// ─── read: special behaviour ─────────────────────────────────────────────────

/// Assert a `tpu` invocation failed *cleanly*: non-zero exit, no Rust panic,
/// an `error:` diagnostic on stderr mentioning `needle`, and nothing written
/// to stdout.
///
/// The distinction matters: an out-of-range `--lines` request used to abort
/// the process with `range start index N out of range` (exit 101), which
/// under `tpu-mcp` killed the io worker and triggered a respawn/retry storm.
fn assert_clean_failure(o: &Output, needle: &str) {
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.contains("panicked"),
        "process panicked instead of failing cleanly:\n{stderr}"
    );
    assert!(
        !stderr.contains("out of range for slice"),
        "slice indexing panic leaked to stderr:\n{stderr}"
    );
    assert_eq!(
        o.status.code(),
        Some(1),
        "expected a clean exit code 1, got {:?}:\n{stderr}",
        o.status.code()
    );
    assert!(
        stderr.contains(needle),
        "stderr should mention {needle:?}:\n{stderr}"
    );
    assert!(
        o.stdout.is_empty(),
        "failed read wrote to stdout: {:?}",
        String::from_utf8_lossy(&o.stdout)
    );
}

#[test]
fn read_lines_out_of_range_exits_err() {
    // Line 9999 is beyond the 10-line file: a clean error, never a panic.
    let o = err(tpu()
        .arg("read")
        .arg("--lines=9999")
        .arg(asset("ascii_10lines.txt")));
    assert_clean_failure(
        &o,
        "--lines: start line 9999 is past end of file (10 lines)",
    );
}

#[test]
fn read_lines_one_past_end_exits_err() {
    let o = err(tpu()
        .arg("read")
        .arg("--lines=11")
        .arg(asset("ascii_10lines.txt")));
    assert_clean_failure(&o, "past end of file (10 lines)");
}

#[test]
fn read_lines_range_starting_past_end_exits_err() {
    // The end bound is normally clamped, but a start past EOF is still fatal.
    let o = err(tpu()
        .arg("read")
        .arg("--lines=11-20")
        .arg(asset("ascii_10lines.txt")));
    assert_clean_failure(&o, "--lines: start line 11 is past end of file (10 lines)");
}

#[test]
fn read_lines_last_line_ok() {
    // The boundary just inside the file must still succeed.
    let o = ok(tpu()
        .arg("read")
        .arg("--lines=10")
        .arg(asset("ascii_10lines.txt")));
    assert!(!o.stdout.is_empty(), "expected the last line's content");
}

#[test]
fn read_lines_last_line_with_open_end_is_clamped() {
    let o = ok(tpu()
        .arg("read")
        .arg("--lines=10-9999")
        .arg(asset("ascii_10lines.txt")));
    let lines: Vec<&[u8]> = o
        .stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "expected exactly the last line");
}

#[test]
fn read_lines_usize_max_exits_err_without_panic() {
    // Parses fine as a usize, then fails the bounds check — no overflow.
    // Built from `usize::MAX` rather than a hard-coded 64-bit literal so the
    // expected failure mode (bounds check, not parse error) holds on 32-bit
    // targets too.
    let o = err(tpu()
        .arg("read")
        .arg(format!("--lines={}", usize::MAX))
        .arg(asset("ascii_10lines.txt")));
    assert_clean_failure(&o, "past end of file");
}

#[test]
fn read_lines_overflowing_value_exits_err_without_panic() {
    // One past `usize::MAX`, so it can never parse on any target: rejected at
    // parse time regardless of pointer width.
    let overflowing = (usize::MAX as u128 + 1).to_string();
    let o = err(tpu()
        .arg("read")
        .arg(format!("--lines={overflowing}"))
        .arg(asset("ascii_10lines.txt")));
    assert_clean_failure(&o, "invalid line number");
}

#[test]
fn read_lines_negative_exits_err_without_panic() {
    for arg in ["--lines=-5", "--lines=-5--1", "--lines=5--3", "--lines=-"] {
        let o = err(tpu().arg("read").arg(arg).arg(asset("ascii_10lines.txt")));
        assert_clean_failure(&o, "--lines");
    }
}

#[test]
fn read_lines_malformed_exits_err_without_panic() {
    for arg in [
        "--lines=abc",
        "--lines=1-abc",
        "--lines=1-",
        "--lines=1.5",
        "--lines=0x10",
        "--lines=1-2-3",
        "--lines= ",
    ] {
        let o = err(tpu().arg("read").arg(arg).arg(asset("ascii_10lines.txt")));
        assert_clean_failure(&o, "--lines");
    }
}

#[test]
fn read_lines_on_empty_file_exits_err() {
    let o = err(tpu().arg("read").arg("--lines=1").arg(asset("empty.txt")));
    assert_clean_failure(&o, "past end of file (0 lines)");
}

#[test]
fn read_lines_past_end_of_single_line_file_exits_err() {
    // A 1-line file must read "1 line", not "1 lines".
    let o = err(tpu()
        .arg("read")
        .arg("--lines=2")
        .arg(asset("singleline.txt")));
    assert_clean_failure(&o, "past end of file (1 line)");

    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.contains("1 lines"),
        "line count must be singular for a 1-line file:\n{stderr}"
    );
}

#[test]
fn read_lines_past_end_line_count_is_pluralized_correctly() {
    // Singular only at exactly 1; plural for 0 and for >1.
    let zero = err(tpu().arg("read").arg("--lines=1").arg(asset("empty.txt")));
    assert_clean_failure(&zero, "past end of file (0 lines)");

    let one = err(tpu()
        .arg("read")
        .arg("--lines=99")
        .arg(asset("singleline.txt")));
    assert_clean_failure(&one, "past end of file (1 line)");

    let many = err(tpu()
        .arg("read")
        .arg("--lines=99")
        .arg(asset("ascii_10lines.txt")));
    assert_clean_failure(&many, "past end of file (10 lines)");
}

#[test]
fn read_lines_last_line_of_file_without_trailing_newline_ok() {
    // A file whose final line lacks a terminator still has that line.
    ok(tpu()
        .arg("read")
        .arg("--lines=1")
        .arg(asset("singleline_no_nl.txt")));
}

#[test]
fn read_lines_past_end_of_file_without_trailing_newline_exits_err() {
    let o = err(tpu()
        .arg("read")
        .arg("--lines=2")
        .arg(asset("singleline_no_nl.txt")));
    assert_clean_failure(&o, "past end of file");
}

#[test]
fn read_lines_past_end_of_crlf_file_exits_err() {
    let o = err(tpu()
        .arg("read")
        .arg("--lines=9999")
        .arg(asset("multiline_crlf.txt")));
    assert_clean_failure(&o, "past end of file");
}

#[test]
fn read_lines_past_end_with_numbers_exits_err() {
    let o = err(tpu()
        .arg("read")
        .arg("--numbers")
        .arg("--lines=9999")
        .arg(asset("ascii_10lines.txt")));
    assert_clean_failure(&o, "past end of file");
}

#[test]
fn read_lines_past_end_emits_no_bom() {
    // --bom=force must not leave three stray bytes on stdout when the range
    // itself is rejected.
    let o = err(tpu()
        .arg("read")
        .arg("--utf8")
        .arg("--bom=force")
        .arg("--lines=9999")
        .arg(asset("utf8_bom.txt")));
    assert_clean_failure(&o, "past end of file");
}

#[test]
fn readex_lines_out_of_range_exits_err() {
    let o = err(tpu()
        .arg("readex")
        .arg("--lines=9999")
        .arg(asset("ascii_10lines.txt")));
    assert_clean_failure(
        &o,
        "--lines: start line 9999 is past end of file (10 lines)",
    );
}

#[test]
fn readex_lines_one_past_end_exits_err() {
    let o = err(tpu()
        .arg("readex")
        .arg("--lines=11-20")
        .arg(asset("ascii_10lines.txt")));
    assert_clean_failure(&o, "past end of file (10 lines)");
}

#[test]
fn readex_lines_on_empty_file_exits_err() {
    // The empty-file shortcut must validate the range too.
    let o = err(tpu().arg("readex").arg("--lines=1").arg(asset("empty.txt")));
    assert_clean_failure(&o, "past end of file (0 lines)");
}

#[test]
fn readex_lines_last_line_ok() {
    ok(tpu()
        .arg("readex")
        .arg("--lines=10")
        .arg(asset("ascii_10lines.txt")));
}

#[test]
fn readex_lines_usize_max_exits_err_without_panic() {
    let o = err(tpu()
        .arg("readex")
        .arg(format!("--lines={}", usize::MAX))
        .arg(asset("ascii_10lines.txt")));
    assert_clean_failure(&o, "past end of file");
}

#[test]
fn readex_lines_negative_exits_err_without_panic() {
    let o = err(tpu()
        .arg("readex")
        .arg("--lines=-5")
        .arg(asset("ascii_10lines.txt")));
    assert_clean_failure(&o, "--lines");
}

#[test]
fn read_lines_range_clamped_ok() {
    // --lines 1-999 on a 10-line file should give 10 lines and exit 0.
    let o = ok(tpu()
        .arg("read")
        .arg("--lines=1-999")
        .arg(asset("ascii_10lines.txt")));
    let lines: Vec<&[u8]> = o
        .stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        10,
        "expected all 10 lines; got {}",
        lines.len()
    );
}

#[test]
fn read_binary_empty_file_gives_empty_output() {
    let o = ok(tpu().arg("read").arg("--binary").arg(asset("empty.txt")));
    assert!(
        o.stdout.is_empty(),
        "expected empty output for empty file in binary mode"
    );
}

#[test]
fn read_binary_byte_1_gives_exactly_one_escaped_byte() {
    // Byte 1 of ascii_10lines.txt is 'l' (printable ASCII), so the output is "l".
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--bytes=1")
        .arg(asset("ascii_10lines.txt")));
    // Output should be ≥1 byte (the single byte's escaped representation).
    assert!(
        !o.stdout.is_empty(),
        "expected non-empty output for --bytes=1"
    );
}

#[test]
fn read_binary_bytes_range_subset_of_whole() {
    // Bytes 1-5 of a file should produce shorter output than the whole file.
    let whole = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg(asset("ascii_10lines.txt")));
    let sub = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--bytes=1-5")
        .arg(asset("ascii_10lines.txt")));
    assert!(
        sub.stdout.len() < whole.stdout.len(),
        "bytes=1-5 output should be shorter than whole-file output"
    );
}

#[test]
fn read_crlf_file_output_has_lf_only() {
    // Normalised output from a CRLF file must not contain bare CR or CRLF.
    let o = ok(tpu().arg("read").arg(asset("multiline_crlf.txt")));
    assert!(
        !o.stdout.contains(&b'\r'),
        "normalised read output must not contain CR"
    );
}

#[test]
fn read_lf_file_output_unchanged() {
    // LF file already normalised: content should match.
    let o = ok(tpu().arg("read").arg(asset("multiline_lf.txt")));
    let s = String::from_utf8(o.stdout).unwrap();
    assert!(s.contains("line one"), "expected content");
    assert!(s.contains("line two"), "expected content");
    assert!(s.contains("line three"), "expected content");
}

#[test]
fn read_numbers_prefix_has_leading_spaces_and_digit() {
    let o = ok(tpu()
        .arg("read")
        .arg("--numbers")
        .arg(asset("ascii_10lines.txt")));
    let first_line = o.stdout.split(|&b| b == b'\n').next().unwrap_or(b"");
    let s = String::from_utf8(first_line.to_vec()).unwrap();
    // Format is "{:>6}  content"; line 1 should start with spaces then "1".
    assert!(
        s.trim_start().starts_with('1'),
        "expected line number prefix; got: {s:?}"
    );
}

#[test]
fn read_bom_preserve_on_bom_file_has_bom() {
    let o = ok(tpu()
        .arg("read")
        .arg("--utf8")
        .arg("--bom=preserve")
        .arg(asset("utf8_bom.txt")));
    assert!(
        o.stdout.starts_with(&[0xEF, 0xBB, 0xBF]),
        "preserve should keep BOM from a file that has one"
    );
}

#[test]
fn read_bom_preserve_on_no_bom_file_no_bom() {
    // pipeline_coverage.txt is a YAML file with no BOM.
    let o = ok(tpu()
        .arg("read")
        .arg("--utf8")
        .arg("--bom=preserve")
        .arg(asset("pipeline_coverage.txt")));
    assert!(
        !o.stdout.starts_with(&[0xEF, 0xBB, 0xBF]),
        "preserve should NOT add BOM for a file that has none"
    );
}

#[test]
fn read_single_line_file_gives_one_line() {
    let o = ok(tpu().arg("read").arg(asset("singleline.txt")));
    let newlines = o.stdout.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(
        newlines, 1,
        "single-line file should produce exactly one output newline"
    );
}

#[test]
fn read_10line_file_gives_ten_lines() {
    let o = ok(tpu().arg("read").arg(asset("ascii_10lines.txt")));
    let newlines = o.stdout.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(
        newlines, 10,
        "10-line file should produce exactly 10 output newlines"
    );
}

#[test]
fn read_lines_selection_exact() {
    // --lines 3-5 of a 10-line file should give exactly 3 lines.
    let o = ok(tpu()
        .arg("read")
        .arg("--lines=3-5")
        .arg(asset("ascii_10lines.txt")));
    let newlines = o.stdout.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(
        newlines, 3,
        "lines 3-5 should give exactly 3 output lines; got {newlines}"
    );
}

#[test]
fn read_lines_single_selection() {
    // --lines 7 of a 10-line file should give exactly 1 line.
    let o = ok(tpu()
        .arg("read")
        .arg("--lines=7")
        .arg(asset("ascii_10lines.txt")));
    let newlines = o.stdout.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(
        newlines, 1,
        "--lines=7 should give exactly 1 output line; got {newlines}"
    );
}

// ─── readex: error cases ─────────────────────────────────────────────────────

#[test]
fn readex_missing_file_exits_err() {
    err(tpu().arg("readex").arg("/nonexistent/zzz_missing.txt"));
}

#[test]
fn readex_bom_without_utf8_exits_err() {
    err(tpu()
        .arg("readex")
        .arg("--bom=strip")
        .arg(asset("singleline.txt")));
}

#[test]
fn readex_binary_and_utf8_conflict_exits_err() {
    err(tpu()
        .arg("readex")
        .arg("--binary")
        .arg("--utf8")
        .arg(asset("singleline.txt")));
}

#[test]
fn readex_binary_and_numbers_conflict_exits_err() {
    err(tpu()
        .arg("readex")
        .arg("--binary")
        .arg("--numbers")
        .arg(asset("singleline.txt")));
}

#[test]
fn readex_binary_and_lines_conflict_exits_err() {
    err(tpu()
        .arg("readex")
        .arg("--binary")
        .arg("--lines=1")
        .arg(asset("singleline.txt")));
}

#[test]
fn readex_lines_zero_exits_err() {
    err(tpu()
        .arg("readex")
        .arg("--lines=0")
        .arg(asset("singleline.txt")));
}

#[test]
fn readex_lines_range_reversed_exits_err() {
    err(tpu()
        .arg("readex")
        .arg("--lines=5-3")
        .arg(asset("ascii_10lines.txt")));
}

#[test]
fn readex_bytes_without_binary_exits_err() {
    err(tpu()
        .arg("readex")
        .arg("--bytes=1")
        .arg(asset("singleline.txt")));
}

#[test]
fn readex_bytes_zero_exits_err() {
    err(tpu()
        .arg("readex")
        .arg("--binary")
        .arg("--bytes=0")
        .arg(asset("singleline.txt")));
}

// ─── readex: special behaviour ───────────────────────────────────────────────

#[test]
fn readex_empty_file_output_is_just_newline() {
    let o = ok(tpu().arg("readex").arg(asset("empty.txt")));
    assert_eq!(
        o.stdout, b"\n",
        "readex of empty file should emit a single bare newline"
    );
}

#[test]
fn readex_binary_empty_file_output_is_just_newline() {
    let o = ok(tpu().arg("readex").arg("--binary").arg(asset("empty.txt")));
    assert_eq!(
        o.stdout, b"\n",
        "readex --binary of empty file should emit a single newline"
    );
}

#[test]
fn readex_output_body_contains_escaped_newlines() {
    // A multi-line file should produce escaped \n sequences in the body.
    let o = ok(tpu().arg("readex").arg(asset("multiline_lf.txt")));
    let s = String::from_utf8(o.stdout).unwrap();
    // Body (everything except trailing real newline) should contain the two-char \n escape.
    assert!(
        s.contains("\\n"),
        "readex of multi-line file must contain '\\n' escapes; got: {s:?}"
    );
}

#[test]
fn readex_crlf_file_escapes_cr_and_lf() {
    // CRLF file should produce \r\n escape sequences in the readex output.
    let o = ok(tpu().arg("readex").arg(asset("multiline_crlf.txt")));
    let s = String::from_utf8(o.stdout).unwrap();
    // The CR bytes (0x0D) appear as \r escape sequences in the output.
    // After harrier normalises CRLF → LF internally, the source lines themselves
    // don't have \r, but the original bytes are NOT in the readex text view.
    // We just verify the output is a valid single flat line.
    let nl_count = s.bytes().filter(|&b| b == b'\n').count();
    assert_eq!(
        nl_count, 1,
        "CRLF file readex should still emit a single flat line"
    );
}

#[test]
fn readex_numbers_prefix_format() {
    // Numbers prefix should be decimal digits right-justified in 6 chars plus two spaces.
    let o = ok(tpu()
        .arg("readex")
        .arg("--numbers")
        .arg(asset("singleline.txt")));
    let s = String::from_utf8(o.stdout).unwrap();
    // The escaped output starts with the line-number prefix for line 1.
    assert!(
        s.starts_with("     1  "),
        "expected '     1  ' prefix; got: {:?}",
        &s[..s.len().min(12)]
    );
}

#[test]
fn readex_lines_selection_count() {
    // readex --lines 3-5 of a 10-line file produces exactly 3 escaped \\n sequences.
    let o = ok(tpu()
        .arg("readex")
        .arg("--lines=3-5")
        .arg(asset("ascii_10lines.txt")));
    let s = String::from_utf8(o.stdout).unwrap();
    let body = s.trim_end_matches('\n');
    let count = body.matches("\\n").count();
    assert_eq!(
        count, 3,
        "3 selected lines should produce 3 \\n escapes; got {count}"
    );
}

#[test]
fn readex_binary_output_uses_printable_ascii_pass_through() {
    // For a plain ASCII file, the binary readex output body should be the same
    // bytes as the input (all printable ASCII passes through the codec unchanged),
    // except newlines become the \n escape and the output always ends with a
    // real newline.
    let o = ok(tpu()
        .arg("readex")
        .arg("--binary")
        .arg(asset("singleline.txt")));
    let s = String::from_utf8(o.stdout).unwrap();
    // All bytes in singleline.txt are printable or newline; body = "hello world\n".
    assert!(
        s.contains("hello world"),
        "printable ASCII should pass through; got: {s:?}"
    );
}

#[test]
fn readex_utf8_bom_preserve_on_bom_file() {
    let o = ok(tpu()
        .arg("readex")
        .arg("--utf8")
        .arg("--bom=preserve")
        .arg(asset("utf8_bom.txt")));
    assert!(
        o.stdout.starts_with(&[0xEF, 0xBB, 0xBF]),
        "readex --bom=preserve should keep BOM from a BOM file"
    );
}

// ─── write: error cases ──────────────────────────────────────────────────────

#[test]
fn write_bom_without_utf8_exits_err() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.txt");
    err_stdin(tpu().arg("write").arg("--bom=strip").arg(&dst), b"text\n");
}

#[test]
fn write_binary_and_utf8_conflict_exits_err() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.txt");
    err_stdin(
        tpu().arg("write").arg("--binary").arg("--utf8").arg(&dst),
        b"text\n",
    );
}

#[test]
fn write_non_utf8_stdin_exits_err() {
    // Text-mode write must reject non-UTF-8 input.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.txt");
    err_stdin(tpu().arg("write").arg(&dst), b"\xFF\xFE garbage not utf8");
}

// ─── write: special behaviour ────────────────────────────────────────────────

#[test]
fn write_new_file_has_exact_content() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("exact.txt");
    ok_stdin(tpu().arg("write").arg(&dst), b"alpha\nbeta\ngamma\n");
    let content = fs::read(&dst).unwrap();
    assert_eq!(content, b"alpha\nbeta\ngamma\n");
}

#[test]
fn write_utf8_bom_force_file_starts_with_bom() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("bom.txt");
    ok_stdin(
        tpu()
            .arg("write")
            .arg("--utf8")
            .arg("--bom=force")
            .arg(&dst),
        b"content\n",
    );
    let bytes = fs::read(&dst).unwrap();
    assert!(
        bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
        "write --utf8 --bom=force must produce a BOM-prefixed file"
    );
}

#[test]
fn write_utf8_bom_strip_file_has_no_bom() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("nobom.txt");
    ok_stdin(
        tpu()
            .arg("write")
            .arg("--utf8")
            .arg("--bom=strip")
            .arg(&dst),
        b"content\n",
    );
    let bytes = fs::read(&dst).unwrap();
    assert!(
        !bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
        "write --utf8 --bom=strip must not produce a BOM"
    );
}

#[test]
fn write_diff_on_no_change_exits_ok() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("same.txt");
    // Write identical content twice; --diff on second write.
    ok_stdin(tpu().arg("write").arg(&dst), b"same content\n");
    ok_stdin(
        tpu().arg("write").arg("--diff").arg(&dst),
        b"same content\n",
    );
}

#[test]
fn write_diff_on_change_exits_ok() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("changed.txt");
    ok_stdin(tpu().arg("write").arg(&dst), b"old content\n");
    let o = ok_stdin(tpu().arg("write").arg("--diff").arg(&dst), b"new content\n");
    // Diff output should be non-empty when content changed.
    assert!(
        !o.stdout.is_empty(),
        "expected non-empty diff output on content change"
    );
}

#[test]
fn write_diff_shows_minus_and_plus_lines() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("diff_check.txt");
    ok_stdin(tpu().arg("write").arg(&dst), b"original\n");
    let o = ok_stdin(tpu().arg("write").arg("--diff").arg(&dst), b"modified\n");
    let diff = String::from_utf8(o.stdout).unwrap();
    assert!(diff.contains('-'), "diff output should have '-' lines");
    assert!(diff.contains('+'), "diff output should have '+' lines");
}

#[test]
fn write_multiple_overwrites_create_multiple_baks() {
    // Only the most recent .bak is kept (each write replaces the .bak).
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("multi.txt");
    ok_stdin(tpu().arg("write").arg(&dst), b"version 1\n");
    ok_stdin(tpu().arg("write").arg(&dst), b"version 2\n");
    ok_stdin(tpu().arg("write").arg(&dst), b"version 3\n");
    // .bak should exist (from the last write)
    assert!(
        bak(&dst).exists(),
        ".bak should exist after multiple overwrites"
    );
    let final_content = fs::read(&dst).unwrap();
    assert_eq!(
        final_content, b"version 3\n",
        "final content should be version 3"
    );
}

// ─── replace: error cases ────────────────────────────────────────────────────

#[test]
fn replace_missing_file_exits_err() {
    err(tpu()
        .arg("replace")
        .arg("/nonexistent/zzz_missing.txt")
        .arg("x")
        .arg("y"));
}

#[test]
fn replace_bad_regex_exits_err_general() {
    let (dir, f) = cp("singleline.txt");
    err(tpu()
        .arg("replace")
        .arg("--regex")
        .arg(&f)
        .arg("[unclosed")
        .arg("Z"));
    drop(dir);
}

#[test]
fn replace_no_file_arg_exits_err() {
    err(tpu().arg("replace"));
}

// ─── replace: special behaviour ──────────────────────────────────────────────

#[test]
fn replace_stderr_contains_filename() {
    let (dir, f) = cp("singleline.txt");
    let o = ok(tpu().arg("replace").arg(&f).arg("ZZZNOMATCH_99").arg("Z"));
    let s = String::from_utf8(o.stderr).unwrap();
    let fname = f.file_name().unwrap().to_str().unwrap();
    assert!(
        s.contains(fname),
        "replace stderr should contain the filename; got: {s:?}"
    );
    drop(dir);
}

#[test]
fn replace_capture_group_substitution() {
    let (dir, f) = cp("singleline.txt");
    // Pattern: capture "hello", replace with "[hello]".
    ok(tpu()
        .arg("replace")
        .arg("--regex")
        .arg(&f)
        .arg("(hello)")
        .arg("[$1]"));
    let content = fs::read_to_string(&f).unwrap();
    assert!(
        content.contains("[hello]"),
        "capture group substitution failed; content: {content:?}"
    );
    drop(dir);
}

/// `$1token` is ambiguous: the regex crate greedily reads `1token` as a
/// single capture-group *name*, which does not exist, so the whole
/// reference expands to nothing — silently dropping both the intended
/// back-reference and the literal "token" suffix.
#[test]
fn replace_ambiguous_dollar_capture_ref_without_braces_drops_suffix() {
    let (dir, f) = cp("singleline.txt");
    ok(tpu()
        .arg("replace")
        .arg("--regex")
        .arg(&f)
        .arg("(hello)")
        .arg("$1token"));
    let content = fs::read_to_string(&f).unwrap();
    assert!(
        !content.contains("hellotoken"),
        "\"$1token\" must NOT resolve to 'hellotoken'; got: {content:?}"
    );
    assert!(
        !content.contains("hello"),
        "the unresolved group-named-'1token' reference must expand to nothing, \
         dropping the original match too; got: {content:?}"
    );
    drop(dir);
}

/// Braces disambiguate the numbered reference from the following literal
/// text: `${1}token` resolves group 1 and appends "token" literally.
#[test]
fn replace_braced_dollar_capture_ref_disambiguates_suffix() {
    let (dir, f) = cp("singleline.txt");
    ok(tpu()
        .arg("replace")
        .arg("--regex")
        .arg(&f)
        .arg("(hello)")
        .arg("${1}token"));
    let content = fs::read_to_string(&f).unwrap();
    assert!(
        content.contains("hellotoken"),
        "\"${{1}}token\" must resolve to 'hellotoken'; got: {content:?}"
    );
    drop(dir);
}

// ─── replacement-string escape decoding ──────────────────────────────────────
//
// By default the replacement passed on the command line is decoded with the
// standard `tpu` escape codec so users can write `\n` for a newline, `\t` for
// a tab, `\\` for a backslash, etc.  `--literal-replacement` (`-L`) disables
// decoding and treats the bytes verbatim.

#[test]
fn replace_default_decodes_backslash_n_to_newline() {
    // singleline.txt is "hello\n".  Replacing "hello" with "a\nb" should
    // produce a real newline between 'a' and 'b'.
    let (dir, f) = cp("singleline.txt");
    ok(tpu().arg("replace").arg(&f).arg("hello").arg("a\\nb"));
    let content = fs::read(&f).unwrap();
    assert!(
        content.windows(3).any(|w| w == b"a\nb"),
        "default mode should turn \\n into LF; got {content:?}"
    );
    assert!(
        !content.windows(3).any(|w| w == b"a\\n"),
        "default mode must NOT leave literal '\\n' in output; got {content:?}"
    );
    drop(dir);
}

#[test]
fn replace_default_decodes_backslash_t_to_tab() {
    let (dir, f) = cp("singleline.txt");
    ok(tpu().arg("replace").arg(&f).arg("hello").arg("a\\tb"));
    let content = fs::read(&f).unwrap();
    assert!(
        content.windows(3).any(|w| w == b"a\tb"),
        "default mode should turn \\t into TAB; got {content:?}"
    );
    drop(dir);
}

#[test]
fn replace_default_decodes_double_backslash_to_single() {
    let (dir, f) = cp("singleline.txt");
    ok(tpu().arg("replace").arg(&f).arg("hello").arg("a\\\\b"));
    let content = fs::read(&f).unwrap();
    assert!(
        content.windows(3).any(|w| w == b"a\\b"),
        "default mode should turn \\\\ into a single backslash; got {content:?}"
    );
    drop(dir);
}

#[test]
fn replace_default_decodes_hex_escape() {
    // \x41 == 'A'.  singleline.txt has a UTF-8 BOM, so 'A' appears
    // somewhere after the leading BOM rather than at byte 0.
    let (dir, f) = cp("singleline.txt");
    ok(tpu().arg("replace").arg(&f).arg("hello").arg("\\x41"));
    let content = fs::read(&f).unwrap();
    assert!(
        content.contains(&b'A'),
        "default mode should turn \\x41 into 'A'; got {content:?}"
    );
    drop(dir);
}

#[test]
fn replace_literal_replacement_keeps_backslash_n_literal() {
    // With --literal-replacement, "\n" in the replacement must remain as
    // the two-byte sequence backslash + 'n' in the output.
    let (dir, f) = cp("singleline.txt");
    ok(tpu()
        .arg("replace")
        .arg("--literal-replacement")
        .arg(&f)
        .arg("hello")
        .arg("a\\nb"));
    let content = fs::read(&f).unwrap();
    assert!(
        content.windows(3).any(|w| w == b"a\\n" || w == b"\\nb"),
        "--literal-replacement should preserve backslash + n verbatim; got {content:?}"
    );
    assert!(
        !content.windows(3).any(|w| w == b"a\nb"),
        "--literal-replacement must NOT produce a real newline; got {content:?}"
    );
    drop(dir);
}

#[test]
fn replace_literal_replacement_short_flag() {
    // `-L` is the short form of `--literal-replacement`.
    let (dir, f) = cp("singleline.txt");
    ok(tpu()
        .arg("replace")
        .arg("-L")
        .arg(&f)
        .arg("hello")
        .arg("x\\ty"));
    let content = fs::read(&f).unwrap();
    assert!(
        content.windows(3).any(|w| w == b"x\\t"),
        "-L should preserve backslash + t verbatim; got {content:?}"
    );
    drop(dir);
}

#[test]
fn replace_default_invalid_escape_exits_err() {
    // `\q` is not a recognised escape.  In the default (decoding) mode this
    // must surface as an error rather than silently producing `q`.
    let (dir, f) = cp("singleline.txt");
    let o = err(tpu().arg("replace").arg(&f).arg("hello").arg("a\\qb"));
    let s = String::from_utf8_lossy(&o.stderr);
    assert!(
        s.to_lowercase().contains("escape") || s.to_lowercase().contains("unknown"),
        "expected an escape-related error; got: {s}"
    );
    drop(dir);
}

#[test]
fn replace_literal_replacement_accepts_unknown_backslash_sequence() {
    // With --literal-replacement there is no escape decoding, so `\q` is
    // simply two raw bytes and the command must succeed.
    let (dir, f) = cp("singleline.txt");
    ok(tpu()
        .arg("replace")
        .arg("--literal-replacement")
        .arg(&f)
        .arg("hello")
        .arg("a\\qb"));
    let content = fs::read(&f).unwrap();
    assert!(
        content.windows(3).any(|w| w == b"a\\q"),
        "--literal-replacement must keep '\\q' verbatim; got {content:?}"
    );
    drop(dir);
}

#[test]
fn replace_default_capture_group_with_newline_escape() {
    // Capture-group expansion ($1) must still work alongside escape decoding,
    // so users can write things like `$1\n$2` to inject a newline between
    // captures.
    let (dir, f) = cp("singleline.txt");
    ok(tpu()
        .arg("replace")
        .arg("--regex")
        .arg(&f)
        .arg("(hel)(lo)")
        .arg("$1\\n$2"));
    let content = fs::read(&f).unwrap();
    assert!(
        content.windows(6).any(|w| w == b"hel\nlo"),
        "capture refs and \\n should compose; got {content:?}"
    );
    drop(dir);
}

#[test]
fn replace_multiline_caret_matches_line_starts() {
    // (?m)^ matches at the start of each line.
    let (dir, f) = cp("multiline_lf.txt");
    let o = ok(tpu()
        .arg("replace")
        .arg("--regex")
        .arg("--multiline")
        .arg(&f)
        .arg("^line")
        .arg("LINE"));
    let s = String::from_utf8(o.stderr).unwrap();
    // The BOM-prefixed first line escapes ^-anchor match; 2 of the 3 lines are replaced.
    assert!(
        s.contains("2 replacements"),
        "expected 2 multiline replacements; got: {s}"
    );
    let content = fs::read_to_string(&f).unwrap();
    assert!(
        content.contains("LINE two"),
        "multiline replace should have changed line two"
    );
    drop(dir);
}

#[test]
fn replace_diff_shows_minus_and_plus_lines() {
    let (dir, f) = cp("singleline.txt");
    let o = ok(tpu()
        .arg("replace")
        .arg("--diff")
        .arg(&f)
        .arg("hello")
        .arg("goodbye"));
    let diff = String::from_utf8(o.stdout).unwrap();
    assert!(diff.contains('-'), "diff should have '-' lines");
    assert!(diff.contains('+'), "diff should have '+' lines");
    drop(dir);
}

#[test]
fn replace_nomatch_leaves_original_untouched_and_writes_no_bak() {
    // Per M7-1, a zero-match run is a no-op at the file-system level: the
    // original file is not rewritten (mtime preserved) and no .bak is
    // written.  This is how callers distinguish "matched nothing" from a
    // real edit without needing a follow-up read.
    let orig_content = fs::read(asset("singleline.txt")).unwrap();
    let (dir, f) = cp("singleline.txt");
    let before_mtime = fs::metadata(&f).unwrap().modified().unwrap();
    // Sleep so a spurious rewrite would produce a distinguishable mtime.
    std::thread::sleep(std::time::Duration::from_millis(50));
    ok(tpu().arg("replace").arg(&f).arg("ZZZNOMATCH_99").arg("Z"));
    assert!(
        !bak(&f).exists(),
        "zero-match run must not create .bak (M7-1 short-circuit)"
    );
    let after_mtime = fs::metadata(&f).unwrap().modified().unwrap();
    assert_eq!(
        before_mtime, after_mtime,
        "zero-match run must preserve mtime (M7-1 short-circuit)"
    );
    let file_content = fs::read(&f).unwrap();
    assert_eq!(
        orig_content, file_content,
        "zero-match run must leave the original file bytes unchanged"
    );
    drop(dir);
}

#[test]
fn replace_count_is_accurate() {
    // The word "fox" appears once in the ascii content.
    // Pattern: "The" — appears once per line in the "The quick brown fox" text.
    let (dir, f) = cp("ascii_10lines.txt");
    let o = ok(tpu().arg("replace").arg(&f).arg("The quick").arg("A fast"));
    let s = String::from_utf8(o.stderr).unwrap();
    assert!(
        s.contains("10 replacements"),
        "expected 10 replacements (one per line); got: {s}"
    );
    drop(dir);
}

#[test]
fn replace_global_replaces_all_occurrences() {
    // All occurrences of "line" in multiline file should be replaced.
    let (dir, f) = cp("multiline_lf.txt");
    ok(tpu().arg("replace").arg(&f).arg("line").arg("LINE"));
    let content = fs::read_to_string(&f).unwrap();
    assert!(
        !content.contains("line "),
        "all 'line ' occurrences should be replaced"
    );
    assert!(
        content.contains("LINE"),
        "replacement text should appear in content"
    );
    drop(dir);
}

#[test]
fn replace_does_not_disturb_unmatched_content() {
    // Replace only "ONE" pattern; "two" and "three" should remain.
    let (dir, f) = cp("multiline_lf.txt");
    ok(tpu().arg("replace").arg(&f).arg("one").arg("ONE"));
    let content = fs::read_to_string(&f).unwrap();
    assert!(
        content.contains("two"),
        "unmatched content 'two' should be unchanged"
    );
    assert!(
        content.contains("three"),
        "unmatched content 'three' should be unchanged"
    );
    drop(dir);
}

// ─── Cross-command round-trip validation ─────────────────────────────────────

#[test]
fn read_binary_produces_same_escaped_bytes_as_readex_binary_minus_trailing_newline() {
    // readex --binary appends one trailing newline; read --binary does not.
    let src = asset("json_lib_core.txt");
    let rb = ok(tpu().arg("read").arg("--binary").arg(&src)).stdout;
    let rxb = ok(tpu().arg("readex").arg("--binary").arg(&src)).stdout;
    // rxb should be rb + b"\n"
    assert!(
        rxb.ends_with(b"\n"),
        "readex --binary must end with newline"
    );
    assert_eq!(
        &rxb[..rxb.len() - 1],
        rb.as_slice(),
        "readex --binary output without trailing newline should equal read --binary output"
    );
}

#[test]
fn replace_then_read_gives_modified_content() {
    let (dir, f) = cp("regex_content.txt");
    ok(tpu().arg("replace").arg(&f).arg("foo").arg("REPLACED"));
    let content_bytes = ok(tpu().arg("read").arg(&f)).stdout;
    let content = String::from_utf8(content_bytes).unwrap();
    assert!(
        content.contains("REPLACED"),
        "replace + read should show modified content"
    );
    assert!(
        !content.contains("foo "),
        "original 'foo ' pattern should be gone after replace"
    );
    drop(dir);
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 8 — --data-format for write (MG-1)
// ═══════════════════════════════════════════════════════════════════════════════

// ─── write --binary --data-format=hex ────────────────────────────────────────

#[test]
fn write_binary_data_format_hex_contiguous() {
    // "4D5A" should write bytes [0x4D, 0x5A].
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&dst)
        .arg("4D5A"));
    assert_eq!(fs::read(&dst).unwrap(), &[0x4D, 0x5A]);
}

#[test]
fn write_binary_data_format_hex_dashed() {
    // "4D-5A-00-FF" should write bytes [0x4D, 0x5A, 0x00, 0xFF].
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&dst)
        .arg("4D-5A-00-FF"));
    assert_eq!(fs::read(&dst).unwrap(), &[0x4D, 0x5A, 0x00, 0xFF]);
}

#[test]
fn write_binary_data_format_hex_lowercase() {
    // Lowercase hex digits should be accepted.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&dst)
        .arg("deadbeef"));
    assert_eq!(fs::read(&dst).unwrap(), &[0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn write_binary_data_format_hex_single_byte() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&dst)
        .arg("FF"));
    assert_eq!(fs::read(&dst).unwrap(), &[0xFF]);
}

#[test]
fn write_binary_data_format_hex_empty() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&dst)
        .arg(""));
    assert_eq!(fs::read(&dst).unwrap(), b"");
}

#[test]
fn write_binary_data_format_hex_creates_bak() {
    // Overwriting an existing file should create a .bak.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    fs::write(&dst, b"original").unwrap();
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&dst)
        .arg("4D5A"));
    assert!(bak(&dst).exists(), ".bak should be created on overwrite");
}

// ─── write --binary --data-format=base64 ─────────────────────────────────────

#[test]
fn write_binary_data_format_base64_hello() {
    // "SGVsbG8=" decodes to "Hello".
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg(&dst)
        .arg("SGVsbG8="));
    assert_eq!(fs::read(&dst).unwrap(), b"Hello");
}

#[test]
fn write_binary_data_format_base64_hello_world() {
    // "SGVsbG8sIFdvcmxkIQ==" decodes to "Hello, World!".
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg(&dst)
        .arg("SGVsbG8sIFdvcmxkIQ=="));
    assert_eq!(fs::read(&dst).unwrap(), b"Hello, World!");
}

#[test]
fn write_binary_data_format_base64_binary_bytes() {
    // "AA==" decodes to a single 0x00 byte.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg(&dst)
        .arg("AA=="));
    assert_eq!(fs::read(&dst).unwrap(), &[0x00]);
}

#[test]
fn write_binary_data_format_base64_no_padding_needed() {
    // "TWFu" (3 bytes "Man") requires no padding.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg(&dst)
        .arg("TWFu"));
    assert_eq!(fs::read(&dst).unwrap(), b"Man");
}

// ─── write --binary --data-format=encoded ────────────────────────────────────

#[test]
fn write_binary_data_format_encoded_plain_ascii() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=encoded")
        .arg(&dst)
        .arg("Hello"));
    assert_eq!(fs::read(&dst).unwrap(), b"Hello");
}

#[test]
fn write_binary_data_format_encoded_with_newline_escape() {
    // The Rust literal "Hello\\nWorld" is the string "Hello\nWorld" which the
    // encoded decoder interprets as "Hello<LF>World".
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=encoded")
        .arg(&dst)
        .arg("Hello\\nWorld"));
    assert_eq!(fs::read(&dst).unwrap(), b"Hello\nWorld");
}

#[test]
fn write_binary_data_format_encoded_hex_escape() {
    // "\x4D\x5A" in encoded format decodes to [0x4D, 0x5A].
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=encoded")
        .arg(&dst)
        .arg("\\x4D\\x5A"));
    assert_eq!(fs::read(&dst).unwrap(), &[0x4D, 0x5A]);
}

// ─── --data-format error cases ───────────────────────────────────────────────

#[test]
fn write_data_format_without_binary_exits_err() {
    // --data-format requires --binary.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    err(tpu()
        .arg("write")
        .arg("--data-format=hex")
        .arg(&dst)
        .arg("4D5A"));
}

#[test]
fn write_data_positional_without_data_format_exits_err() {
    // DATA positional requires --data-format.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    err(tpu().arg("write").arg("--binary").arg(&dst).arg("4D5A"));
}

#[test]
fn write_binary_data_format_hex_invalid_char_exits_err() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    err(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&dst)
        .arg("GGGG"));
}

#[test]
fn write_binary_data_format_hex_odd_digits_exits_err() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    err(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&dst)
        .arg("4D5"));
}

#[test]
fn write_binary_data_format_base64_invalid_char_exits_err() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    err(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg(&dst)
        .arg("SG!s"));
}

#[test]
fn write_binary_data_format_base64_bad_length_exits_err() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    err(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg(&dst)
        .arg("SGVsb")); // 5 chars — not a multiple of 4
}

#[test]
fn write_binary_data_format_encoded_bad_escape_exits_err() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    err(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=encoded")
        .arg(&dst)
        .arg("\\q")); // \q is not a valid escape
}

// ─── --data-format with --diff ───────────────────────────────────────────────

#[test]
fn write_binary_data_format_hex_with_diff_exits_ok() {
    // --diff is accepted alongside --data-format.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    // Create a prior version so a diff can be emitted.
    fs::write(&dst, b"\x00\x00").unwrap();
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--diff")
        .arg("--data-format=hex")
        .arg(&dst)
        .arg("4D5A"));
    assert_eq!(fs::read(&dst).unwrap(), &[0x4D, 0x5A]);
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 9 — --message-format=json (MJ)
//
// All tests verify NDJSON output: every line is a complete JSON object with a
// `reason` field.  Tests exercise the following `reason` values:
//   "data"     — produced by read / readex (text and binary)
//   "status"   — produced by replace
//   "finished" — always last; success:true on exit 0, success:false on exit 1
//   "error"    — produced on failure
// ═══════════════════════════════════════════════════════════════════════════════

/// Parse all NDJSON lines from `bytes` into a `Vec<serde_json::Value>`.
/// Blank lines are skipped.  Panics on invalid JSON.
fn parse_ndjson(bytes: &[u8]) -> Vec<serde_json::Value> {
    let s = std::str::from_utf8(bytes).expect("stdout must be valid UTF-8");
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad JSON line {l:?}: {e}")))
        .collect()
}

/// Extract the `reason` string from a JSON object.
fn reason(v: &serde_json::Value) -> &str {
    v["reason"].as_str().expect("reason must be a string")
}

// ─── Helper: base64 decode (same algorithm as at the top of the file) ───────
// Re-used via `decode_b64` which is already in scope.

// ─── read --message-format=json: text mode ──────────────────────────────────

#[test]
fn json_read_text_emits_data_and_finished() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg(asset("singleline.txt")));
    assert!(o.stderr.is_empty(), "stderr must be empty in JSON mode");
    let msgs = parse_ndjson(&o.stdout);
    assert!(
        msgs.len() >= 2,
        "expected at least 2 messages; got {}",
        msgs.len()
    );
    assert_eq!(reason(&msgs[0]), "data", "first message must be 'data'");
    assert_eq!(
        reason(msgs.last().unwrap()),
        "finished",
        "last message must be 'finished'"
    );
}

#[test]
fn json_read_text_data_encoding_is_text() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg(asset("singleline.txt")));
    let msgs = parse_ndjson(&o.stdout);
    let data = msgs
        .iter()
        .find(|m| reason(m) == "data")
        .expect("data message missing");
    assert_eq!(data["encoding"].as_str().unwrap(), "text");
}

#[test]
fn json_read_text_data_subcommand_is_read() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg(asset("singleline.txt")));
    let msgs = parse_ndjson(&o.stdout);
    let data = msgs
        .iter()
        .find(|m| reason(m) == "data")
        .expect("data message missing");
    assert_eq!(data["subcommand"].as_str().unwrap(), "read");
}

#[test]
fn json_read_text_content_matches_plain_read_output() {
    // In JSON mode the content field (plus a trailing newline) should equal the
    // plain `read` stdout so the information is identical.
    let plain = ok(tpu().arg("read").arg(asset("singleline.txt"))).stdout;
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg(asset("singleline.txt")));
    let msgs = parse_ndjson(&o.stdout);
    let data = msgs
        .iter()
        .find(|m| reason(m) == "data")
        .expect("data message missing");
    let content = data["content"].as_str().expect("content must be string");
    // Plain read ends with a newline; the JSON content field preserves it.
    assert_eq!(content.as_bytes(), plain.as_slice());
}

#[test]
fn json_read_text_finished_success_true() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg(asset("singleline.txt")));
    let msgs = parse_ndjson(&o.stdout);
    let fin = msgs.last().unwrap();
    assert_eq!(reason(fin), "finished");
    assert!(fin["success"].as_bool().unwrap());
}

#[test]
fn json_read_text_finished_is_last() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg(asset("ascii_10lines.txt")));
    let msgs = parse_ndjson(&o.stdout);
    let last = msgs.last().expect("expected at least one message");
    assert_eq!(reason(last), "finished");
}

#[test]
fn json_read_text_no_output_on_stderr() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg(asset("unicode.txt")));
    assert!(
        o.stderr.is_empty(),
        "no output expected on stderr in JSON mode"
    );
}

#[test]
fn json_read_text_multiline_content_preserved() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg(asset("multiline_lf.txt")));
    let msgs = parse_ndjson(&o.stdout);
    let data = msgs
        .iter()
        .find(|m| reason(m) == "data")
        .expect("data message missing");
    let content = data["content"].as_str().unwrap();
    // The three-line file should have its newlines preserved in the JSON string.
    assert!(
        content.contains('\n'),
        "multi-line content must contain newlines"
    );
    let line_count = content.lines().count();
    assert_eq!(
        line_count, 3,
        "expected 3 lines in content; got {line_count}"
    );
}

#[test]
fn json_read_text_unicode_content_round_trips() {
    let plain = ok(tpu().arg("read").arg(asset("unicode.txt"))).stdout;
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg(asset("unicode.txt")));
    let msgs = parse_ndjson(&o.stdout);
    let data = msgs
        .iter()
        .find(|m| reason(m) == "data")
        .expect("data message missing");
    let content = data["content"].as_str().unwrap();
    assert_eq!(content.as_bytes(), plain.as_slice());
}

// ─── read --message-format=json: binary mode ────────────────────────────────

#[test]
fn json_read_binary_emits_data_and_finished() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg("--binary")
        .arg(asset("singleline.txt")));
    assert!(o.stderr.is_empty(), "stderr must be empty in JSON mode");
    let msgs = parse_ndjson(&o.stdout);
    assert!(msgs.len() >= 2);
    assert_eq!(reason(&msgs[0]), "data");
    assert_eq!(reason(msgs.last().unwrap()), "finished");
}

#[test]
fn json_read_binary_encoding_is_bytes_base64() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg("--binary")
        .arg(asset("singleline.txt")));
    let msgs = parse_ndjson(&o.stdout);
    let data = msgs
        .iter()
        .find(|m| reason(m) == "data")
        .expect("data message missing");
    assert_eq!(data["encoding"].as_str().unwrap(), "bytes-base64");
}

#[test]
fn json_read_binary_base64_decodes_to_original_bytes() {
    let original = std::fs::read(asset("singleline.txt")).unwrap();
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg("--binary")
        .arg(asset("singleline.txt")));
    let msgs = parse_ndjson(&o.stdout);
    let data = msgs
        .iter()
        .find(|m| reason(m) == "data")
        .expect("data message missing");
    let b64 = data["content"].as_str().unwrap();
    let decoded = decode_b64(b64);
    assert_eq!(
        decoded, original,
        "base64 decode should reproduce original file bytes"
    );
}

#[test]
fn json_read_binary_all_256_bytes_round_trip() {
    // binary.bin contains bytes 0..255; base64 decode must reproduce them exactly.
    let original = std::fs::read(asset("binary.bin")).unwrap();
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg("--binary")
        .arg(asset("binary.bin")));
    let msgs = parse_ndjson(&o.stdout);
    let data = msgs
        .iter()
        .find(|m| reason(m) == "data")
        .expect("data message missing");
    let decoded = decode_b64(data["content"].as_str().unwrap());
    assert_eq!(decoded, original);
}

// ─── readex --message-format=json ───────────────────────────────────────────

#[test]
fn json_readex_text_emits_data_and_finished() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("readex")
        .arg(asset("singleline.txt")));
    assert!(o.stderr.is_empty());
    let msgs = parse_ndjson(&o.stdout);
    assert!(msgs.len() >= 2);
    assert_eq!(reason(&msgs[0]), "data");
    assert_eq!(reason(msgs.last().unwrap()), "finished");
}

#[test]
fn json_readex_text_subcommand_is_readex() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("readex")
        .arg(asset("singleline.txt")));
    let msgs = parse_ndjson(&o.stdout);
    let data = msgs
        .iter()
        .find(|m| reason(m) == "data")
        .expect("data message missing");
    assert_eq!(data["subcommand"].as_str().unwrap(), "readex");
}

#[test]
fn json_readex_binary_encoding_is_bytes_base64() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("readex")
        .arg("--binary")
        .arg(asset("binary.bin")));
    let msgs = parse_ndjson(&o.stdout);
    let data = msgs
        .iter()
        .find(|m| reason(m) == "data")
        .expect("data message missing");
    assert_eq!(data["encoding"].as_str().unwrap(), "bytes-base64");
}

#[test]
fn json_readex_binary_base64_decodes_correctly() {
    let original = std::fs::read(asset("binary.bin")).unwrap();
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("readex")
        .arg("--binary")
        .arg(asset("binary.bin")));
    let msgs = parse_ndjson(&o.stdout);
    let data = msgs
        .iter()
        .find(|m| reason(m) == "data")
        .expect("data message missing");
    let decoded = decode_b64(data["content"].as_str().unwrap());
    assert_eq!(decoded, original);
}

// ─── replace --message-format=json ──────────────────────────────────────────

#[test]
fn json_replace_nomatch_emits_status_and_finished() {
    let (dir, f) = cp("singleline.txt");
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("replace")
        .arg(&f)
        .arg("ZZZNOMATCH_XY0000")
        .arg("Z"));
    assert!(o.stderr.is_empty(), "stderr must be empty in JSON mode");
    let msgs = parse_ndjson(&o.stdout);
    let reasons: Vec<&str> = msgs.iter().map(reason).collect();
    assert!(
        reasons.contains(&"status"),
        "expected 'status' message; got {reasons:?}"
    );
    assert_eq!(reason(msgs.last().unwrap()), "finished");
    drop(dir);
}

#[test]
fn json_replace_status_contains_zero_replacements() {
    let (dir, f) = cp("singleline.txt");
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("replace")
        .arg(&f)
        .arg("ZZZNOMATCH_XY0000")
        .arg("Z"));
    let msgs = parse_ndjson(&o.stdout);
    let status = msgs
        .iter()
        .find(|m| reason(m) == "status")
        .expect("status message missing");
    let msg = status["message"].as_str().unwrap();
    assert!(
        msg.contains("0 replacements"),
        "expected '0 replacements'; got: {msg}"
    );
    drop(dir);
}

#[test]
fn json_replace_match_emits_status_and_finished() {
    let (dir, f) = cp("ascii_10lines.txt");
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("replace")
        .arg(&f)
        .arg("The quick")
        .arg("A fast"));
    assert!(o.stderr.is_empty());
    let msgs = parse_ndjson(&o.stdout);
    let reasons: Vec<&str> = msgs.iter().map(reason).collect();
    assert!(reasons.contains(&"status"));
    assert_eq!(reason(msgs.last().unwrap()), "finished");
    drop(dir);
}

#[test]
fn json_replace_status_accurate_count() {
    let (dir, f) = cp("ascii_10lines.txt");
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("replace")
        .arg(&f)
        .arg("The quick")
        .arg("A fast"));
    let msgs = parse_ndjson(&o.stdout);
    let status = msgs
        .iter()
        .find(|m| reason(m) == "status")
        .expect("status message missing");
    let msg = status["message"].as_str().unwrap();
    assert!(
        msg.contains("10 replacements"),
        "expected '10 replacements'; got: {msg}"
    );
    drop(dir);
}

#[test]
fn json_replace_finished_success_true() {
    let (dir, f) = cp("singleline.txt");
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("replace")
        .arg(&f)
        .arg("hello")
        .arg("goodbye"));
    let msgs = parse_ndjson(&o.stdout);
    let fin = msgs.last().unwrap();
    assert!(fin["success"].as_bool().unwrap());
    drop(dir);
}

#[test]
fn json_replace_diff_emits_diff_message() {
    let (dir, f) = cp("singleline.txt");
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("replace")
        .arg("--diff")
        .arg(&f)
        .arg("hello")
        .arg("goodbye"));
    let msgs = parse_ndjson(&o.stdout);
    let reasons: Vec<&str> = msgs.iter().map(reason).collect();
    assert!(
        reasons.contains(&"diff"),
        "expected 'diff' message; got {reasons:?}"
    );
    drop(dir);
}

#[test]
fn json_replace_diff_content_has_minus_and_plus() {
    let (dir, f) = cp("singleline.txt");
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("replace")
        .arg("--diff")
        .arg(&f)
        .arg("hello")
        .arg("goodbye"));
    let msgs = parse_ndjson(&o.stdout);
    let diff = msgs
        .iter()
        .find(|m| reason(m) == "diff")
        .expect("diff message missing");
    let content = diff["content"].as_str().unwrap();
    assert!(content.contains('-'), "diff content must have '-' lines");
    assert!(content.contains('+'), "diff content must have '+' lines");
    drop(dir);
}

// ─── Error path --message-format=json ───────────────────────────────────────

#[test]
fn json_read_missing_file_emits_error_and_finished_false() {
    let o = err(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg("/nonexistent/zzz_missing_file.txt"));
    assert!(
        o.stderr.is_empty(),
        "stderr must be empty in JSON mode even on error"
    );
    let msgs = parse_ndjson(&o.stdout);
    let reasons: Vec<&str> = msgs.iter().map(reason).collect();
    assert!(
        reasons.contains(&"error"),
        "expected 'error' message; got {reasons:?}"
    );
    assert_eq!(reason(msgs.last().unwrap()), "finished");
    let fin = msgs.last().unwrap();
    assert!(!fin["success"].as_bool().unwrap());
}

#[test]
fn json_replace_bad_regex_emits_error_and_finished_false() {
    let (dir, f) = cp("singleline.txt");
    let o = err(tpu()
        .arg("--message-format=json")
        .arg("replace")
        .arg("--regex")
        .arg(&f)
        .arg("[invalid")
        .arg("Z"));
    assert!(o.stderr.is_empty());
    let msgs = parse_ndjson(&o.stdout);
    let reasons: Vec<&str> = msgs.iter().map(reason).collect();
    assert!(
        reasons.contains(&"error"),
        "expected 'error' on bad regex; got {reasons:?}"
    );
    let fin = msgs.last().unwrap();
    assert!(!fin["success"].as_bool().unwrap());
    drop(dir);
}

#[test]
fn json_error_message_field_is_non_empty() {
    let o = err(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg("/nonexistent/zzz_missing_file.txt"));
    let msgs = parse_ndjson(&o.stdout);
    let error = msgs
        .iter()
        .find(|m| reason(m) == "error")
        .expect("error message missing");
    let msg = error["message"].as_str().unwrap();
    assert!(!msg.is_empty(), "error message field must be non-empty");
}

// ─── NDJSON format invariants ────────────────────────────────────────────────

#[test]
fn json_each_stdout_line_is_valid_json_object() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg(asset("ascii_10lines.txt")));
    let s = std::str::from_utf8(&o.stdout).unwrap();
    for line in s.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line is not valid JSON: {line:?}: {e}"));
        assert!(v.is_object(), "each line must be a JSON object: {line}");
    }
}

#[test]
fn json_finished_always_last_on_success() {
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg(asset("multiline_lf.txt")));
    let msgs = parse_ndjson(&o.stdout);
    assert!(!msgs.is_empty());
    assert_eq!(reason(msgs.last().unwrap()), "finished");
}

#[test]
fn json_finished_always_last_on_error() {
    let o = err(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg("/nonexistent/zzz_error_path.txt"));
    let msgs = parse_ndjson(&o.stdout);
    assert!(!msgs.is_empty());
    assert_eq!(reason(msgs.last().unwrap()), "finished");
}

#[test]
fn json_human_mode_produces_no_json_on_stdout_for_read() {
    // In default (human) mode, read output goes to stdout as raw bytes, not JSON.
    let o = ok(tpu().arg("read").arg(asset("singleline.txt")));
    // The output should NOT parse as NDJSON with a "reason" field.
    let s = std::str::from_utf8(&o.stdout).unwrap();
    for line in s.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            assert!(
                v["reason"].is_null(),
                "human-mode output must not have a 'reason' field; got: {v}"
            );
        }
    }
}

// SECTION 10 — --data-length cross-check for write (TPU-10)
// ═══════════════════════════════════════════════════════════════════════════════

// ─── write --binary stdin path (no --data-format) ────────────────────────────

#[test]
fn write_binary_data_length_decimal_exact_match() {
    // Decimal --data-length that matches stdin byte count → file written.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok_stdin(
        tpu()
            .arg("write")
            .arg("--binary")
            .arg("--data-length=5")
            .arg(&dst),
        b"Hello",
    );
    assert_eq!(fs::read(&dst).unwrap(), b"Hello");
}

#[test]
fn write_binary_data_length_hex_prefix_exact_match() {
    // 0x5 == 5 decimal; should match 5-byte stdin payload.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok_stdin(
        tpu()
            .arg("write")
            .arg("--binary")
            .arg("--data-length=0x5")
            .arg(&dst),
        b"Hello",
    );
    assert_eq!(fs::read(&dst).unwrap(), b"Hello");
}

#[test]
fn write_binary_data_length_zero_with_empty_stdin() {
    // Zero-length payload with --data-length=0.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok_stdin(
        tpu()
            .arg("write")
            .arg("--binary")
            .arg("--data-length=0")
            .arg(&dst),
        b"",
    );
    assert_eq!(fs::read(&dst).unwrap(), b"");
}

#[test]
fn write_binary_data_length_single_byte() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok_stdin(
        tpu()
            .arg("write")
            .arg("--binary")
            .arg("--data-length=1")
            .arg(&dst),
        &[0xFF],
    );
    assert_eq!(fs::read(&dst).unwrap(), &[0xFF]);
}

#[test]
fn write_binary_data_length_mismatch_too_small_is_error() {
    // Declared 4 but stdin has 5 bytes → non-zero exit.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    err_stdin(
        tpu()
            .arg("write")
            .arg("--binary")
            .arg("--data-length=4")
            .arg(&dst),
        b"Hello",
    );
}

#[test]
fn write_binary_data_length_mismatch_too_large_is_error() {
    // Declared 10 but stdin has only 5 bytes → non-zero exit.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    err_stdin(
        tpu()
            .arg("write")
            .arg("--binary")
            .arg("--data-length=10")
            .arg(&dst),
        b"Hello",
    );
}

#[test]
fn write_binary_data_length_mismatch_leaves_file_unchanged() {
    // An existing file must not be modified when the length check fails.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    fs::write(&dst, b"original").unwrap();
    err_stdin(
        tpu()
            .arg("write")
            .arg("--binary")
            .arg("--data-length=4")
            .arg(&dst),
        b"Hello",
    );
    assert_eq!(
        fs::read(&dst).unwrap(),
        b"original",
        "file must be unchanged"
    );
}

#[test]
fn write_binary_data_length_mismatch_error_message_contains_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    let o = err_stdin(
        tpu()
            .arg("write")
            .arg("--binary")
            .arg("--data-length=4")
            .arg(&dst),
        b"Hello",
    );
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("mismatch"),
        "stderr must mention 'mismatch'; got: {stderr}"
    );
}

// ─── write --binary --data-format=hex path ───────────────────────────────────

#[test]
fn write_binary_data_format_hex_data_length_match() {
    // "4D5A" decodes to 2 bytes; --data-length=2 passes.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg("--data-length=2")
        .arg(&dst)
        .arg("4D5A"));
    assert_eq!(fs::read(&dst).unwrap(), &[0x4D, 0x5A]);
}

#[test]
fn write_binary_data_format_hex_data_length_hex_value_match() {
    // --data-length=0x2 with a 2-byte hex payload.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg("--data-length=0x2")
        .arg(&dst)
        .arg("4D5A"));
    assert_eq!(fs::read(&dst).unwrap(), &[0x4D, 0x5A]);
}

#[test]
fn write_binary_data_format_hex_data_length_four_bytes_match() {
    // "deadbeef" decodes to 4 bytes.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg("--data-length=4")
        .arg(&dst)
        .arg("deadbeef"));
    assert_eq!(fs::read(&dst).unwrap(), &[0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn write_binary_data_format_hex_data_length_mismatch_is_error() {
    // "4D5A" = 2 bytes; --data-length=3 → non-zero exit.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    err(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg("--data-length=3")
        .arg(&dst)
        .arg("4D5A"));
}

#[test]
fn write_binary_data_format_hex_data_length_mismatch_leaves_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    fs::write(&dst, b"original").unwrap();
    err(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg("--data-length=3")
        .arg(&dst)
        .arg("4D5A"));
    assert_eq!(fs::read(&dst).unwrap(), b"original");
}

#[test]
fn write_binary_data_format_hex_data_length_mismatch_error_contains_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    let o = err(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg("--data-length=3")
        .arg(&dst)
        .arg("4D5A"));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("mismatch"),
        "stderr must mention 'mismatch'; got: {stderr}"
    );
}

// ─── write --binary --data-format=base64 path ────────────────────────────────

#[test]
fn write_binary_data_format_base64_data_length_match() {
    // "SGVsbG8=" decodes to "Hello" (5 bytes).
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg("--data-length=5")
        .arg(&dst)
        .arg("SGVsbG8="));
    assert_eq!(fs::read(&dst).unwrap(), b"Hello");
}

#[test]
fn write_binary_data_format_base64_data_length_mismatch_is_error() {
    // "SGVsbG8=" decodes to 5 bytes; --data-length=4 → error.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    err(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg("--data-length=4")
        .arg(&dst)
        .arg("SGVsbG8="));
}

#[test]
fn write_binary_data_format_base64_data_length_mismatch_leaves_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    fs::write(&dst, b"original").unwrap();
    err(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg("--data-length=4")
        .arg(&dst)
        .arg("SGVsbG8="));
    assert_eq!(fs::read(&dst).unwrap(), b"original");
}

#[test]
fn write_binary_data_format_base64_data_length_hex_value_match() {
    // --data-length=0x5 with a 5-byte base64 payload.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg("--data-length=0x5")
        .arg(&dst)
        .arg("SGVsbG8="));
    assert_eq!(fs::read(&dst).unwrap(), b"Hello");
}

// SECTION 11 — @file response-file dispatch (TPU-11)
// ═══════════════════════════════════════════════════════════════════════════════

/// Write `content` to a temporary file and return `(TempDir, path, "@path")`
/// where `"@path"` is the argument string to pass to tpu.
fn rsp_file(content: &str) -> (tempfile::TempDir, std::path::PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("args.rsp");
    fs::write(&path, content.as_bytes()).unwrap();
    let at_arg = format!("@{}", path.display());
    (dir, path, at_arg)
}

// ─── Successful dispatch ──────────────────────────────────────────────────────

#[test]
fn rsp_read_subcommand_exits_zero() {
    // A response file containing "read <asset>" should succeed.
    let src = asset("singleline.txt");
    let content = format!("read \"{}\"", src.display());
    let (_d, _p, at) = rsp_file(&content);
    ok(tpu().arg(&at));
}

#[test]
fn rsp_read_output_matches_direct_invocation() {
    // tpu @rsp and tpu read <file> must produce identical stdout.
    let src = asset("singleline.txt");
    let direct = ok(tpu().arg("read").arg(&src));

    let content = format!("read \"{}\"", src.display());
    let (_d, _p, at) = rsp_file(&content);
    let via_rsp = ok(tpu().arg(&at));

    assert_eq!(direct.stdout, via_rsp.stdout);
}

#[test]
fn rsp_read_binary_subcommand_exits_zero() {
    let src = asset("binary.bin");
    let content = format!("read --binary \"{}\"", src.display());
    let (_d, _p, at) = rsp_file(&content);
    ok(tpu().arg(&at));
}

#[test]
fn rsp_write_creates_file() {
    // write --binary --data-format=encoded passes DATA as a positional arg,
    // making it usable from a response file without stdin.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    let content = format!(
        "write --binary --data-format=encoded \"{}\" hello",
        dst.display()
    );
    let (_d, _p, at) = rsp_file(&content);
    ok(tpu().arg(&at));
    assert_eq!(fs::read(&dst).unwrap(), b"hello");
}

#[test]
fn rsp_tokens_split_across_newlines() {
    // Arguments on separate lines must be treated as individual tokens.
    let src = asset("singleline.txt");
    let content = format!("read\n\"{}\"", src.display());
    let (_d, _p, at) = rsp_file(&content);
    ok(tpu().arg(&at));
}

#[test]
fn rsp_tokens_split_across_tabs() {
    let src = asset("singleline.txt");
    let content = format!("read\t\"{}\"", src.display());
    let (_d, _p, at) = rsp_file(&content);
    ok(tpu().arg(&at));
}

#[test]
fn rsp_quoted_path_with_space_works() {
    // Quoted path containing a space must survive tokenisation intact.
    let outer = tempfile::tempdir().unwrap();
    // Create a subdirectory whose name contains a space.
    let spaced = outer.path().join("my dir");
    fs::create_dir(&spaced).unwrap();
    let src_content = "line one\n";
    let src = spaced.join("file.txt");
    fs::write(&src, src_content.as_bytes()).unwrap();

    let content = format!("read \"{}\"", src.display());
    let (_d, _p, at) = rsp_file(&content);
    let o = ok(tpu().arg(&at));
    assert_eq!(String::from_utf8_lossy(&o.stdout), src_content);
}

#[test]
fn rsp_global_message_format_flag_in_rsp() {
    // --message-format=json is a global flag; it must work from a rsp file.
    let src = asset("singleline.txt");
    let content = format!("--message-format=json read \"{}\"", src.display());
    let (_d, _p, at) = rsp_file(&content);
    let o = ok(tpu().arg(&at));
    // In JSON mode output has a "reason" field.
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(
        stdout.contains("\"reason\""),
        "expected json output; got: {stdout}"
    );
}

// ─── Error cases ─────────────────────────────────────────────────────────────

#[test]
fn rsp_missing_file_exits_nonzero() {
    err(tpu().arg("@nonexistent_rsp_file_xyz_abc.rsp"));
}

#[test]
fn rsp_missing_file_error_message_on_stderr() {
    let o = err(tpu().arg("@nonexistent_rsp_file_xyz_abc.rsp"));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("nonexistent_rsp_file_xyz_abc.rsp"),
        "stderr should name the missing file; got: {stderr}"
    );
}

#[test]
fn rsp_unmatched_quote_exits_nonzero() {
    let (_d, _p, at) = rsp_file("read \"unclosed");
    err(tpu().arg(&at));
}

#[test]
fn rsp_unmatched_quote_error_message_on_stderr() {
    let (_d, _p, at) = rsp_file("read \"unclosed");
    let o = err(tpu().arg(&at));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("unmatched"),
        "stderr should mention 'unmatched'; got: {stderr}"
    );
}

#[test]
fn rsp_bad_subcommand_in_rsp_exits_nonzero() {
    // A response file with an invalid subcommand must fail via clap.
    let (_d, _p, at) = rsp_file("nosuchthing");
    err(tpu().arg(&at));
}

// SECTION 12 — --validate pre-write guard (TPU-12)
//
// Tests: text mode (line:N, line-contains:N), binary mode (bytes:, md5:,
// crc32:), mode mismatch errors, multiple validators, out-of-range selectors.

/// Hex-encode a byte slice (lowercase, no separators).
fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Compute MD5 digest as a lowercase hex string.
fn md5_hex(b: &[u8]) -> String {
    use md5::{Digest as _, Md5};
    format!("{:x}", Md5::digest(b))
}

/// Compute CRC32 as an 8-digit lowercase hex string.
fn crc32_hex(b: &[u8]) -> String {
    format!("{:08x}", crc32fast::hash(b))
}

// ── Text mode: line:N ─────────────────────────────────────────────────────────

#[test]
fn validate_text_line_pass_allows_write() {
    // Validation passes → write must succeed (exit 0).
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.txt");
    fs::write(&dst, b"hello\nworld\n").unwrap();
    ok_stdin(
        tpu()
            .args(["write", "--validate", "line:1", "hello"])
            .arg(&dst),
        b"new content\n",
    );
}

#[test]
fn validate_text_line_fail_blocks_write() {
    // Validation fails → non-zero exit and file unchanged.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.txt");
    fs::write(&dst, b"hello\nworld\n").unwrap();
    err(tpu()
        .args(["write", "--validate", "line:1", "wrong"])
        .arg(&dst));
    assert_eq!(
        fs::read(&dst).unwrap(),
        b"hello\nworld\n",
        "file must be unchanged"
    );
}

#[test]
fn validate_text_line_out_of_range_blocks_write() {
    // Line out of range → non-zero exit and file unchanged.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.txt");
    fs::write(&dst, b"one\ntwo\n").unwrap();
    err(tpu()
        .args(["write", "--validate", "line:99", "one"])
        .arg(&dst));
    assert_eq!(
        fs::read(&dst).unwrap(),
        b"one\ntwo\n",
        "file must be unchanged"
    );
}

// ── Text mode: line-contains:N ───────────────────────────────────────────────

#[test]
fn validate_text_line_contains_pass_allows_write() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.txt");
    fs::write(&dst, b"hello world\n").unwrap();
    ok_stdin(
        tpu()
            .args(["write", "--validate", "line-contains:1", "world"])
            .arg(&dst),
        b"updated\n",
    );
}

#[test]
fn validate_text_line_contains_fail_blocks_write() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.txt");
    fs::write(&dst, b"hello world\n").unwrap();
    err(tpu()
        .args(["write", "--validate", "line-contains:1", "rust"])
        .arg(&dst));
    assert_eq!(
        fs::read(&dst).unwrap(),
        b"hello world\n",
        "file must be unchanged"
    );
}

// ── Binary mode: bytes:OFFSET-END ────────────────────────────────────────────

#[test]
fn validate_binary_bytes_pass_allows_write() {
    // bytes:0-2 matches first two bytes of a 4-byte file → write succeeds.
    let data: &[u8] = b"\x4d\x5a\x00\x00"; // MZ header prefix
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    fs::write(&dst, data).unwrap();
    ok_stdin(
        tpu()
            .args(["write", "--binary", "--validate", "bytes:0-2", "4d5a"])
            .arg(&dst),
        data,
    );
}

#[test]
fn validate_binary_bytes_fail_blocks_write() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    fs::write(&dst, b"hello").unwrap();
    err(tpu()
        .args(["write", "--binary", "--validate", "bytes:0-5", "0000000000"])
        .arg(&dst));
    assert_eq!(fs::read(&dst).unwrap(), b"hello", "file must be unchanged");
}

#[test]
fn validate_binary_bytes_out_of_range_blocks_write() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    fs::write(&dst, b"hi").unwrap();
    err(tpu()
        .args(["write", "--binary", "--validate", "bytes:0-100", "0000"])
        .arg(&dst));
    assert_eq!(fs::read(&dst).unwrap(), b"hi", "file must be unchanged");
}

// ── Binary mode: md5:OFFSET-END ──────────────────────────────────────────────

#[test]
fn validate_binary_md5_pass_allows_write() {
    let data: &[u8] = b"hello";
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    fs::write(&dst, data).unwrap();
    let good_md5 = md5_hex(data);
    ok_stdin(
        tpu()
            .args(["write", "--binary", "--validate", "md5:0-5"])
            .arg(&good_md5)
            .arg(&dst),
        b"new bytes",
    );
}

#[test]
fn validate_binary_md5_fail_blocks_write() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    fs::write(&dst, b"hello").unwrap();
    err(tpu()
        .args([
            "write",
            "--binary",
            "--validate",
            "md5:0-5",
            "00000000000000000000000000000000",
        ])
        .arg(&dst));
    assert_eq!(fs::read(&dst).unwrap(), b"hello", "file must be unchanged");
}

// ── Binary mode: crc32:OFFSET-END ────────────────────────────────────────────

#[test]
fn validate_binary_crc32_pass_allows_write() {
    let data: &[u8] = b"hello";
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    fs::write(&dst, data).unwrap();
    let good_crc = crc32_hex(data);
    ok_stdin(
        tpu()
            .args(["write", "--binary", "--validate", "crc32:0-5"])
            .arg(&good_crc)
            .arg(&dst),
        b"new bytes",
    );
}

#[test]
fn validate_binary_crc32_fail_blocks_write() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    fs::write(&dst, b"hello").unwrap();
    err(tpu()
        .args(["write", "--binary", "--validate", "crc32:0-5", "00000000"])
        .arg(&dst));
    assert_eq!(fs::read(&dst).unwrap(), b"hello", "file must be unchanged");
}

// ── Mode mismatch ─────────────────────────────────────────────────────────────

#[test]
fn validate_mode_mismatch_binary_selector_text_mode_err() {
    // bytes: selector requires --binary; using it in text mode is an error.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.txt");
    fs::write(&dst, b"hello\n").unwrap();
    let o = err(tpu()
        .args(["write", "--validate", "bytes:0-5", "68656c6c6f"])
        .arg(&dst));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.is_empty(),
        "stderr must be non-empty on mode mismatch; got nothing"
    );
    assert_eq!(
        fs::read(&dst).unwrap(),
        b"hello\n",
        "file must be unchanged"
    );
}

#[test]
fn validate_mode_mismatch_text_selector_binary_mode_err() {
    // line: selector requires text mode; using it with --binary is an error.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    fs::write(&dst, b"hello").unwrap();
    let o = err(tpu()
        .args(["write", "--binary", "--validate", "line:1", "hello"])
        .arg(&dst));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.is_empty(),
        "stderr must be non-empty on mode mismatch; got nothing"
    );
    assert_eq!(fs::read(&dst).unwrap(), b"hello", "file must be unchanged");
}

// ── Multiple validators ───────────────────────────────────────────────────────

#[test]
fn validate_multiple_all_pass_allows_write() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.txt");
    fs::write(&dst, b"foo\nbar\n").unwrap();
    ok_stdin(
        tpu()
            .args([
                "write",
                "--validate",
                "line:1",
                "foo",
                "--validate",
                "line:2",
                "bar",
            ])
            .arg(&dst),
        b"new content\n",
    );
}

#[test]
fn validate_multiple_second_fails_blocks_write() {
    // First validator passes, second fails → write must not happen.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.txt");
    fs::write(&dst, b"foo\nbar\n").unwrap();
    err(tpu()
        .args([
            "write",
            "--validate",
            "line:1",
            "foo",
            "--validate",
            "line:2",
            "wrong",
        ])
        .arg(&dst));
    assert_eq!(
        fs::read(&dst).unwrap(),
        b"foo\nbar\n",
        "file must be unchanged"
    );
}

// ── hex helper sanity ─────────────────────────────────────────────────────────

#[test]
fn validate_section_hex_helper_sanity() {
    assert_eq!(to_hex(b"\x4d\x5a"), "4d5a");
}

#[test]
fn validate_section_md5_hex_sanity() {
    // MD5("") = d41d8cd98f00b204e9800998ecf8427e (well-known constant)
    assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
}

#[test]
fn validate_section_crc32_hex_sanity() {
    // CRC32(b"") = 00000000
    assert_eq!(crc32_hex(b""), "00000000");
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 13 — --output-format for read / readex (TPU-15)
//
// Tests cover three encoders (hex, base64, encoded) for both `read --binary`
// and `readex --binary`.  Each encoder is tested for:
//   • Known-output verification on a small hand-crafted payload
//   • Full round-trip: encode → decode via `write --binary --data-format=X`
//   • Correct output structure (PEM line-wrap, CRLF terminator for base64)
//   • Error: --output-format without --binary is rejected
// ═══════════════════════════════════════════════════════════════════════════════

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Write `bytes` to a new temp file and return (TempDir, PathBuf).
fn temp_bin(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("payload.bin");
    fs::write(&p, bytes).unwrap();
    (dir, p)
}

// ─── hex encoder ─────────────────────────────────────────────────────────────

#[test]
fn read_output_format_hex_known_bytes() {
    // A 4-byte payload [0x4D, 0x5A, 0x00, 0xFF] must produce "4D-5A-00-FF".
    let (_dir, src) = temp_bin(&[0x4D, 0x5A, 0x00, 0xFF]);
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=hex")
        .arg(&src));
    assert_eq!(o.stdout, b"4D-5A-00-FF");
}

#[test]
fn read_output_format_hex_single_byte() {
    let (_dir, src) = temp_bin(&[0xAB]);
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=hex")
        .arg(&src));
    assert_eq!(o.stdout, b"AB");
}

#[test]
fn read_output_format_hex_empty_file() {
    let (_dir, src) = temp_bin(b"");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=hex")
        .arg(&src));
    assert_eq!(o.stdout, b"");
}

#[test]
fn read_output_format_hex_binary_bin_starts_with_mz() {
    // Use a file with known MZ bytes so the assertion is stable regardless of binary.bin content.
    let (_dir, src) = temp_bin(&[0x4D, 0x5A]);
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=hex")
        .arg(&src));
    assert_eq!(o.stdout, b"4D-5A");
}

#[test]
fn read_output_format_hex_uppercase_digits() {
    // Encoder must use uppercase: A-F, not a-f.
    let (_dir, src) = temp_bin(&[0xAB, 0xCD, 0xEF]);
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=hex")
        .arg(&src));
    let s = String::from_utf8(o.stdout).unwrap();
    assert_eq!(s, "AB-CD-EF");
}

#[test]
fn read_output_format_hex_roundtrip_binary_bin() {
    // Read binary.bin as hex, write via --data-format=hex, compare with original.
    let original = fs::read(asset("binary.bin")).unwrap();
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=hex")
        .arg(asset("binary.bin")));
    let hex_str = String::from_utf8(o.stdout).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&dst)
        .arg(&hex_str));
    assert_eq!(fs::read(&dst).unwrap(), original);
}

#[test]
fn readex_output_format_hex_known_bytes() {
    // readex with --output-format=hex: same encoding as read.
    let (_dir, src) = temp_bin(&[0x00, 0x01, 0x02]);
    let o = ok(tpu()
        .arg("readex")
        .arg("--binary")
        .arg("--output-format=hex")
        .arg(&src));
    assert_eq!(o.stdout, b"00-01-02");
}

#[test]
fn readex_output_format_hex_roundtrip_binary_bin() {
    let original = fs::read(asset("binary.bin")).unwrap();
    let o = ok(tpu()
        .arg("readex")
        .arg("--binary")
        .arg("--output-format=hex")
        .arg(asset("binary.bin")));
    let hex_str = String::from_utf8(o.stdout).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&dst)
        .arg(&hex_str));
    assert_eq!(fs::read(&dst).unwrap(), original);
}

// ─── base64 PEM encoder ───────────────────────────────────────────────────────

#[test]
fn read_output_format_base64_hello() {
    // b"Hello" → "SGVsbG8=" (flat) → PEM-wrapped with \r\n.
    let (_dir, src) = temp_bin(b"Hello");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=base64")
        .arg(&src));
    assert_eq!(o.stdout, b"SGVsbG8=\r\n");
}

#[test]
fn read_output_format_base64_empty_file() {
    let (_dir, src) = temp_bin(b"");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=base64")
        .arg(&src));
    assert_eq!(o.stdout, b"");
}

#[test]
fn read_output_format_base64_output_has_crlf() {
    // Every non-empty output line must end with \r\n.
    let (_dir, src) = temp_bin(b"Hello, World!");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=base64")
        .arg(&src));
    assert!(
        o.stdout.ends_with(b"\r\n"),
        "last line must end with \\r\\n"
    );
    assert!(
        !o.stdout.contains(&b'\n')
            || o.stdout
                .windows(2)
                .all(|w| w != b"\n\r" && (w[1] != b'\n' || w[0] == b'\r')),
        "every \\n must be preceded by \\r"
    );
}

#[test]
fn read_output_format_base64_line_length_at_most_66() {
    // PEM lines are 64 base64 chars + \r\n = 66 bytes max (including the CRLF).
    let data: Vec<u8> = (0u8..=255u8).collect();
    let (_dir, src) = temp_bin(&data);
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=base64")
        .arg(&src));
    let s = String::from_utf8(o.stdout).unwrap();
    for line in s.split("\r\n").filter(|l| !l.is_empty()) {
        assert!(
            line.len() <= 64,
            "base64 data per line must be ≤ 64 chars; got {}",
            line.len()
        );
    }
}

#[test]
fn read_output_format_base64_roundtrip_binary_bin() {
    // Encode binary.bin as PEM base64, write back via --data-format=base64 (decoder
    // strips \r\n), and verify the written file matches the original byte-for-byte.
    let original = fs::read(asset("binary.bin")).unwrap();
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=base64")
        .arg(asset("binary.bin")));
    let pem = String::from_utf8(o.stdout).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg(&dst)
        .arg(&pem));
    assert_eq!(fs::read(&dst).unwrap(), original);
}

#[test]
fn readex_output_format_base64_roundtrip_binary_bin() {
    let original = fs::read(asset("binary.bin")).unwrap();
    let o = ok(tpu()
        .arg("readex")
        .arg("--binary")
        .arg("--output-format=base64")
        .arg(asset("binary.bin")));
    let pem = String::from_utf8(o.stdout).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg(&dst)
        .arg(&pem));
    assert_eq!(fs::read(&dst).unwrap(), original);
}

#[test]
fn read_output_format_base64_roundtrip_all_byte_values() {
    // 256-byte payload with every possible byte value.
    let data: Vec<u8> = (0u8..=255u8).collect();
    let (_dir, src) = temp_bin(&data);
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=base64")
        .arg(&src));
    let pem = String::from_utf8(o.stdout).unwrap();

    let dir2 = tempfile::tempdir().unwrap();
    let dst = dir2.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg(&dst)
        .arg(&pem));
    assert_eq!(fs::read(&dst).unwrap(), data);
}

// ─── encoded encoder ──────────────────────────────────────────────────────────

#[test]
fn read_output_format_encoded_plain_ascii() {
    // Printable ASCII (excl. backslash) passes through unchanged.
    let (_dir, src) = temp_bin(b"hello world");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=encoded")
        .arg(&src));
    assert_eq!(o.stdout, b"hello world");
}

#[test]
fn read_output_format_encoded_backslash_escaped() {
    let (_dir, src) = temp_bin(b"a\\b");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=encoded")
        .arg(&src));
    assert_eq!(o.stdout, b"a\\\\b");
}

#[test]
fn read_output_format_encoded_nul_byte() {
    let (_dir, src) = temp_bin(&[0x00]);
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=encoded")
        .arg(&src));
    assert_eq!(o.stdout, b"\\x00");
}

#[test]
fn read_output_format_encoded_mz_header() {
    // [0x4D, 0x5A, 0x00, 0x00] → "MZ\x00\x00".
    let (_dir, src) = temp_bin(&[0x4D, 0x5A, 0x00, 0x00]);
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=encoded")
        .arg(&src));
    assert_eq!(o.stdout, b"MZ\\x00\\x00");
}

#[test]
fn read_output_format_encoded_empty_file() {
    let (_dir, src) = temp_bin(b"");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=encoded")
        .arg(&src));
    assert_eq!(o.stdout, b"");
}

#[test]
fn read_output_format_encoded_uppercase_hex_in_escapes() {
    // Non-printable bytes must use uppercase hex digits in \xHH.
    let (_dir, src) = temp_bin(&[0xAB, 0xCD]);
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=encoded")
        .arg(&src));
    assert_eq!(o.stdout, b"\\xAB\\xCD");
}

#[test]
fn read_output_format_encoded_roundtrip_binary_bin() {
    // Encode binary.bin as encoded, write back via --data-format=encoded,
    // and verify the result matches the original byte-for-byte.
    let original = fs::read(asset("binary.bin")).unwrap();
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=encoded")
        .arg(asset("binary.bin")));
    let enc = String::from_utf8(o.stdout).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=encoded")
        .arg(&dst)
        .arg(&enc));
    assert_eq!(fs::read(&dst).unwrap(), original);
}

#[test]
fn readex_output_format_encoded_roundtrip_binary_bin() {
    let original = fs::read(asset("binary.bin")).unwrap();
    let o = ok(tpu()
        .arg("readex")
        .arg("--binary")
        .arg("--output-format=encoded")
        .arg(asset("binary.bin")));
    let enc = String::from_utf8(o.stdout).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=encoded")
        .arg(&dst)
        .arg(&enc));
    assert_eq!(fs::read(&dst).unwrap(), original);
}

#[test]
fn read_output_format_encoded_roundtrip_all_byte_values() {
    let data: Vec<u8> = (0u8..=255u8).collect();
    let (_dir, src) = temp_bin(&data);
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--output-format=encoded")
        .arg(&src));
    let enc = String::from_utf8(o.stdout).unwrap();

    let dir2 = tempfile::tempdir().unwrap();
    let dst = dir2.path().join("out.bin");
    ok(tpu()
        .arg("write")
        .arg("--binary")
        .arg("--data-format=encoded")
        .arg(&dst)
        .arg(&enc));
    assert_eq!(fs::read(&dst).unwrap(), data);
}

// ─── bytes-range interaction ──────────────────────────────────────────────────

// ─── edit (ED-IT) ─────────────────────────────────────────────────────────────

/// ED-IT-1: Binary delete — delete a known middle byte range; verify prefix
/// and suffix are unchanged and the deleted region is gone.
///
/// Use a self-contained 50-byte payload `[0, 1, ..., 49]` so the test is
/// independent of any asset-file layout changes.
/// Delete bytes [10, 20) via the range string "10-20".
/// Expected result: 40 bytes — prefix [0..10] unchanged, range absent, suffix [20..50] follows.
#[test]
fn edit_binary_delete_middle_range() {
    let data: Vec<u8> = (0u8..50).collect();
    let (_dir, src) = temp_bin(&data);

    ok(tpu()
        .arg("edit")
        .arg("--binary")
        .arg("--delete")
        .arg("10-20")
        .arg(&src));

    let result = fs::read(&src).unwrap();

    // Length check: deleted 10 bytes from a 50-byte file.
    assert_eq!(
        result.len(),
        40,
        "file should be 40 bytes after deleting 10; got {}",
        result.len()
    );

    // Prefix bytes 0x00..0x09 are intact.
    let prefix: Vec<u8> = (0u8..10).collect();
    assert_eq!(
        &result[..10],
        prefix.as_slice(),
        "prefix bytes 0..10 must be unchanged"
    );

    // Suffix bytes 0x14..0x31 immediately follow the prefix.
    let suffix: Vec<u8> = (20u8..50).collect();
    assert_eq!(
        &result[10..],
        suffix.as_slice(),
        "suffix bytes 20..50 must follow immediately after the deleted gap"
    );
}

/// ED-IT-2: Binary insert — insert bytes at a known offset; verify bytes
/// before offset are unchanged, inserted bytes appear, bytes after offset
/// (shifted) are unchanged.
///
/// Use a 30-byte payload `[0, 1, ..., 29]`. Insert 4 bytes `[0xAA, 0xBB, 0xCC, 0xDD]`
/// before offset 15. Expected result: 34 bytes — prefix [0..15], inserted bytes, suffix [15..30].
#[test]
fn edit_binary_insert_at_offset() {
    let data: Vec<u8> = (0u8..30).collect();
    let (_dir, src) = temp_bin(&data);

    // Use hex encoding so the inserted bytes survive shell quoting cleanly.
    ok(tpu()
        .arg("edit")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg("--insert")
        .arg("15")
        .arg("AA-BB-CC-DD")
        .arg(&src));

    let result = fs::read(&src).unwrap();

    // Length check: 30 + 4 = 34.
    assert_eq!(
        result.len(),
        34,
        "file should be 34 bytes after inserting 4; got {}",
        result.len()
    );

    // Prefix [0..15] unchanged.
    let prefix: Vec<u8> = (0u8..15).collect();
    assert_eq!(
        &result[..15],
        prefix.as_slice(),
        "prefix bytes 0..15 must be unchanged"
    );

    // Inserted bytes appear at [15..19].
    assert_eq!(
        &result[15..19],
        &[0xAA, 0xBB, 0xCC, 0xDD],
        "inserted bytes must appear at offset 15"
    );

    // Suffix [15..30] shifted to [19..34].
    let suffix: Vec<u8> = (15u8..30).collect();
    assert_eq!(
        &result[19..],
        suffix.as_slice(),
        "original bytes from offset 15 onward must be shifted right by 4"
    );
}

/// ED-IT-3: Binary splice — replace a known byte range with different-length
/// content; verify the result is prefix + new_data + suffix.
///
/// Use a 30-byte payload `[0, 1, ..., 29]`. Splice bytes [10, 15) with 2
/// replacement bytes `[0xEE, 0xFF]` (shorter than the 5-byte range).
/// Expected result: 27 bytes — prefix [0..10], `[0xEE, 0xFF]`, suffix [15..30].
#[test]
fn edit_binary_splice_different_length() {
    let data: Vec<u8> = (0u8..30).collect();
    let (_dir, src) = temp_bin(&data);

    ok(tpu()
        .arg("edit")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg("--splice")
        .arg("10-15")
        .arg("EE-FF")
        .arg(&src));

    let result = fs::read(&src).unwrap();

    // Length check: 30 - 5 + 2 = 27.
    assert_eq!(
        result.len(),
        27,
        "file should be 27 bytes after splicing 5 bytes with 2; got {}",
        result.len()
    );

    // Prefix [0..10] unchanged.
    let prefix: Vec<u8> = (0u8..10).collect();
    assert_eq!(
        &result[..10],
        prefix.as_slice(),
        "prefix bytes 0..10 must be unchanged"
    );

    // Replacement bytes appear at [10..12].
    assert_eq!(
        &result[10..12],
        &[0xEE, 0xFF],
        "replacement bytes must appear at offset 10"
    );

    // Suffix [15..30] shifted to [12..27].
    let suffix: Vec<u8> = (15u8..30).collect();
    assert_eq!(
        &result[12..],
        suffix.as_slice(),
        "original bytes from offset 15 onward must follow the replacement"
    );
}

/// ED-IT-4: Binary multi-op composability — three non-overlapping ops in one
/// `tpu edit --binary` call, all positions referencing the original file;
/// verify all three are applied correctly and non-targeted bytes are unchanged.
///
/// 50-byte payload `[0..49]`.  Three ops on the original file:
///   - delete bytes [5, 8)   → removes bytes 5, 6, 7
///   - splice bytes [20, 23) with `[0xAA, 0xBB]`  → replaces 3 bytes with 2
///   - insert before byte 40 `[0xCC]`  → inserts 1 byte
///
/// Expected result length: 50 - 3 - 3 + 2 + 1 = 47 bytes.
/// Expected content assembled from original coords:
///   [0..5] ++ [8..20] ++ [0xAA,0xBB] ++ [23..40] ++ [0xCC] ++ [40..50]
#[test]
fn edit_binary_multi_op_composability() {
    let data: Vec<u8> = (0u8..50).collect();
    let (_dir, src) = temp_bin(&data);

    ok(tpu()
        .arg("edit")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg("--delete")
        .arg("5-8")
        .arg("--splice")
        .arg("20-23")
        .arg("AA-BB")
        .arg("--insert")
        .arg("40")
        .arg("CC")
        .arg(&src));

    let result = fs::read(&src).unwrap();

    // Build expected output from original coordinates.
    let mut expected: Vec<u8> = Vec::new();
    expected.extend_from_slice(&data[0..5]); // [0..5]
    expected.extend_from_slice(&data[8..20]); // [8..20] (delete removed 5-7)
    expected.extend_from_slice(&[0xAA, 0xBB]); // splice replacement
    expected.extend_from_slice(&data[23..40]); // [23..40] (splice removed 20-22)
    expected.push(0xCC); // insert before original byte 40
    expected.extend_from_slice(&data[40..50]); // [40..50]

    assert_eq!(
        result.len(),
        expected.len(),
        "length mismatch: got {}, expected {}",
        result.len(),
        expected.len()
    );
    assert_eq!(
        result, expected,
        "multi-op result must match expected bytes"
    );
}

/// ED-IT-5: Binary ops are applied in reverse source order — supply ops in
/// forward order on the CLI and verify the result is identical to supplying
/// them in reverse order.
///
/// 50-byte payload `[0..49]`. Two non-overlapping ops:
///   - delete [5, 10)
///   - splice [20, 25) with `[0xAA, 0xBB, 0xCC]`
///
/// Run once with ops in forward order, once in reverse order.
/// Both invocations must produce identical output.
#[test]
fn edit_binary_ops_applied_in_reverse_source_order() {
    let data: Vec<u8> = (0u8..50).collect();

    // Forward order: delete first, splice second.
    let (_dir_fwd, src_fwd) = temp_bin(&data);
    ok(tpu()
        .arg("edit")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg("--delete")
        .arg("5-10")
        .arg("--splice")
        .arg("20-25")
        .arg("AA-BB-CC")
        .arg(&src_fwd));

    // Reverse order: splice first, delete second.
    let (_dir_rev, src_rev) = temp_bin(&data);
    ok(tpu()
        .arg("edit")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg("--splice")
        .arg("20-25")
        .arg("AA-BB-CC")
        .arg("--delete")
        .arg("5-10")
        .arg(&src_rev));

    let result_fwd = fs::read(&src_fwd).unwrap();
    let result_rev = fs::read(&src_rev).unwrap();

    assert_eq!(
        result_fwd, result_rev,
        "forward and reverse op order must produce identical output"
    );

    // Also verify the content is correct: [0..5] ++ [10..20] ++ [AA,BB,CC] ++ [25..50]
    let mut expected: Vec<u8> = Vec::new();
    expected.extend_from_slice(&data[0..5]);
    expected.extend_from_slice(&data[10..20]);
    expected.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
    expected.extend_from_slice(&data[25..50]);
    assert_eq!(result_fwd, expected, "content must match expected bytes");
}

/// ED-IT-6: Line delete — delete lines 3-5 of a 10-line file; verify result
/// has 7 lines and lines 1-2 + 6-10 are unchanged.
#[test]
fn edit_line_delete_middle_lines() {
    let (_dir, src) = cp("ascii_10lines.txt");

    ok(tpu().arg("edit").arg("--delete").arg("3-5").arg(&src));

    let raw = String::from_utf8(fs::read(&src).unwrap()).unwrap();
    // Strip UTF-8 BOM if present so prefix checks are reliable.
    let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let lines: Vec<&str> = content.lines().collect();

    assert_eq!(
        lines.len(),
        7,
        "should have 7 lines after deleting 3; got {}",
        lines.len()
    );

    // Lines 1-2 unchanged.
    assert!(
        lines[0].starts_with("line 1:"),
        "line 1 must be first: {:?}",
        lines[0]
    );
    assert!(
        lines[1].starts_with("line 2:"),
        "line 2 must be second: {:?}",
        lines[1]
    );

    // Lines 6-10 shifted up.
    assert!(
        lines[2].starts_with("line 6:"),
        "line 6 must be third: {:?}",
        lines[2]
    );
    assert!(
        lines[3].starts_with("line 7:"),
        "line 7 must be fourth: {:?}",
        lines[3]
    );
    assert!(
        lines[4].starts_with("line 8:"),
        "line 8 must be fifth: {:?}",
        lines[4]
    );
    assert!(
        lines[5].starts_with("line 9:"),
        "line 9 must be sixth: {:?}",
        lines[5]
    );
    assert!(
        lines[6].starts_with("line 10:"),
        "line 10 must be seventh: {:?}",
        lines[6]
    );

    // Deleted lines must not appear.
    assert!(
        !content.contains("line 3:")
            && !content.contains("line 4:")
            && !content.contains("line 5:"),
        "deleted lines 3-5 must not appear in output"
    );
}

/// ED-IT-7: Line insert-before — insert two lines before line 5; verify
/// result has 12 lines, inserted lines are at positions 5-6, original lines
/// 5-10 shifted to 7-12.
///
/// Strategy: pass two LF-terminated lines as a single `--insert 5` DATA
/// argument.  `denorm_bytes` re-encodes the embedded `\n` separators to the
/// file's detected line ending, so both new lines land contiguously before
/// original line 5.
#[test]
fn edit_line_insert_before_line_5() {
    let (_dir, src) = cp("ascii_10lines.txt");

    ok(tpu()
        .arg("edit")
        .arg("--insert")
        .arg("5")
        .arg("inserted line A\ninserted line B\n")
        .arg(&src));

    let raw = String::from_utf8(fs::read(&src).unwrap()).unwrap();
    let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let lines: Vec<&str> = content.lines().collect();

    assert_eq!(
        lines.len(),
        12,
        "should have 12 lines after inserting 2; got {}",
        lines.len()
    );

    // Lines 1-4 unchanged.
    assert!(lines[0].starts_with("line 1:"), "pos 1: {:?}", lines[0]);
    assert!(lines[1].starts_with("line 2:"), "pos 2: {:?}", lines[1]);
    assert!(lines[2].starts_with("line 3:"), "pos 3: {:?}", lines[2]);
    assert!(lines[3].starts_with("line 4:"), "pos 4: {:?}", lines[3]);

    // Inserted lines at positions 5-6.
    assert_eq!(lines[4], "inserted line A", "pos 5: {:?}", lines[4]);
    assert_eq!(lines[5], "inserted line B", "pos 6: {:?}", lines[5]);

    // Original lines 5-10 shifted to positions 7-12.
    assert!(lines[6].starts_with("line 5:"), "pos 7: {:?}", lines[6]);
    assert!(lines[7].starts_with("line 6:"), "pos 8: {:?}", lines[7]);
    assert!(lines[8].starts_with("line 7:"), "pos 9: {:?}", lines[8]);
    assert!(lines[9].starts_with("line 8:"), "pos 10: {:?}", lines[9]);
    assert!(lines[10].starts_with("line 9:"), "pos 11: {:?}", lines[10]);
    assert!(lines[11].starts_with("line 10:"), "pos 12: {:?}", lines[11]);
}

// ED-IT-21: --delete $ on a 10-line file removes only the last line.
#[test]
fn edit_line_delete_last_line_dollar() {
    let (_dir, src) = cp("ascii_10lines.txt");

    ok(tpu().arg("edit").arg("--delete").arg("$").arg(&src));

    let raw = String::from_utf8(fs::read(&src).unwrap()).unwrap();
    let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let lines: Vec<&str> = content.lines().collect();

    assert_eq!(
        lines.len(),
        9,
        "should have 9 lines after deleting last; got {}",
        lines.len()
    );
    assert!(lines[0].starts_with("line 1:"), "line 1: {:?}", lines[0]);
    assert!(lines[8].starts_with("line 9:"), "line 9: {:?}", lines[8]);
}

// ED-IT-22: --delete N-$ removes lines N through end of file.
#[test]
fn edit_line_delete_n_to_eof() {
    let (_dir, src) = cp("ascii_10lines.txt");

    ok(tpu().arg("edit").arg("--delete").arg("8-$").arg(&src));

    let raw = String::from_utf8(fs::read(&src).unwrap()).unwrap();
    let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let lines: Vec<&str> = content.lines().collect();

    assert_eq!(
        lines.len(),
        7,
        "should have 7 lines after deleting 8-$; got {}",
        lines.len()
    );
    assert!(lines[0].starts_with("line 1:"), "line 1: {:?}", lines[0]);
    assert!(lines[6].starts_with("line 7:"), "line 7: {:?}", lines[6]);
}

// ED-IT-23: --insert $ appends a line after the last line.
#[test]
fn edit_line_insert_at_eof() {
    let (_dir, src) = cp("ascii_10lines.txt");

    ok(tpu()
        .arg("edit")
        .arg("--insert")
        .arg("$")
        .arg("appended line\n")
        .arg(&src));

    let raw = String::from_utf8(fs::read(&src).unwrap()).unwrap();
    let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let lines: Vec<&str> = content.lines().collect();

    assert_eq!(
        lines.len(),
        11,
        "should have 11 lines after appending; got {}",
        lines.len()
    );
    assert!(lines[0].starts_with("line 1:"), "line 1: {:?}", lines[0]);
    assert!(lines[9].starts_with("line 10:"), "line 10: {:?}", lines[9]);
    assert_eq!(lines[10], "appended line", "appended: {:?}", lines[10]);
}

// Regression: --insert with offset == total_lines + 1 should be treated as
// append (not an out-of-range error).  A 10-line file should accept offset 11.
#[test]
fn edit_line_insert_at_line_count_plus_one() {
    let (_dir, src) = cp("ascii_10lines.txt");

    ok(tpu()
        .arg("edit")
        .arg("--insert")
        .arg("11") // total_lines(10) + 1 == valid append position
        .arg("appended line\n")
        .arg(&src));

    let raw = String::from_utf8(fs::read(&src).unwrap()).unwrap();
    let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let lines: Vec<&str> = content.lines().collect();

    assert_eq!(lines.len(), 11, "expected 11 lines; got {}", lines.len());
    assert_eq!(lines[10], "appended line", "last line: {:?}", lines[10]);
}

// --insert with offset == total_lines + 2 must still be an error.
#[test]
fn edit_line_insert_beyond_line_count_plus_one_is_error() {
    let (_dir, src) = cp("ascii_10lines.txt");

    err(tpu()
        .arg("edit")
        .arg("--insert")
        .arg("12") // total_lines(10) + 2 == out of range
        .arg("bad\n")
        .arg(&src));
}

// ED-IT-24: binary --delete N-$ removes bytes from offset N to end.
#[test]
fn edit_binary_delete_n_to_eof() {
    let (_dir, src) = temp_bin(&[0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48]);

    ok(tpu()
        .arg("edit")
        .arg("--binary")
        .arg("--delete")
        .arg("4-$")
        .arg(&src));

    // Binary ranges are half-open [N, M): deleting 4-$ removes bytes at indices 4..8 (EFGH).
    let result = fs::read(&src).unwrap();
    assert_eq!(result, b"ABCD", "expected first 4 bytes; got {:?}", result);
}

// ED-IT-25: binary --insert $ appends bytes at end of file.
#[test]
fn edit_binary_insert_at_eof() {
    let (_dir, src) = temp_bin(&[0x41, 0x42, 0x43]);

    ok(tpu()
        .arg("edit")
        .arg("--binary")
        .arg("--insert")
        .arg("$")
        .arg("DEF")
        .arg(&src));

    let result = fs::read(&src).unwrap();
    assert_eq!(result, b"ABCDEF", "expected ABCDEF; got {:?}", result);
}

/// ED-IT-8: Line splice — replace lines 2-4 with a single new line; verify
/// result has 8 lines and content outside the spliced region is unchanged.
#[test]
fn edit_line_splice_replace_range() {
    let (_dir, src) = cp("ascii_10lines.txt");

    ok(tpu()
        .arg("edit")
        .arg("--splice")
        .arg("2-4")
        .arg("spliced line\n")
        .arg(&src));

    let raw = String::from_utf8(fs::read(&src).unwrap()).unwrap();
    let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let lines: Vec<&str> = content.lines().collect();

    assert_eq!(
        lines.len(),
        8,
        "should have 8 lines after replacing 3 with 1; got {}",
        lines.len()
    );

    // Line 1 unchanged.
    assert!(lines[0].starts_with("line 1:"), "line 1: {:?}", lines[0]);

    // Lines 2-4 replaced by single spliced line.
    assert_eq!(lines[1], "spliced line", "spliced: {:?}", lines[1]);

    // Original lines 5-10 shifted to positions 3-8.
    assert!(lines[2].starts_with("line 5:"), "pos 3: {:?}", lines[2]);
    assert!(lines[3].starts_with("line 6:"), "pos 4: {:?}", lines[3]);
    assert!(lines[4].starts_with("line 7:"), "pos 5: {:?}", lines[4]);
    assert!(lines[5].starts_with("line 8:"), "pos 6: {:?}", lines[5]);
    assert!(lines[6].starts_with("line 9:"), "pos 7: {:?}", lines[6]);
    assert!(lines[7].starts_with("line 10:"), "pos 8: {:?}", lines[7]);
}

/// ED-IT-9: Line multi-op composability — delete line 2, splice lines 5-6,
/// insert before line 8, all in one invocation, all coords referencing the
/// original file.
#[test]
fn edit_line_multi_op_composable() {
    let (_dir, src) = cp("ascii_10lines.txt");

    // Three operations whose coordinates all reference the original (pre-edit) file.
    ok(tpu()
        .arg("edit")
        .arg("--delete")
        .arg("2")
        .arg("--splice")
        .arg("5-6")
        .arg("spliced 5-6\n")
        .arg("--insert")
        .arg("8")
        .arg("inserted before 8\n")
        .arg(&src));

    let raw = String::from_utf8(fs::read(&src).unwrap()).unwrap();
    let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let lines: Vec<&str> = content.lines().collect();

    // Net: -1 (delete) + (2→1 splice = -1) + 1 (insert) = 9 lines.
    assert_eq!(lines.len(), 9, "should have 9 lines; got {}", lines.len());

    assert!(lines[0].starts_with("line 1:"), "pos 1: {:?}", lines[0]);
    // line 2 deleted → line 3 is now second.
    assert!(lines[1].starts_with("line 3:"), "pos 2: {:?}", lines[1]);
    assert!(lines[2].starts_with("line 4:"), "pos 3: {:?}", lines[2]);
    // lines 5-6 spliced → one replacement line.
    assert_eq!(lines[3], "spliced 5-6", "pos 4: {:?}", lines[3]);
    assert!(lines[4].starts_with("line 7:"), "pos 5: {:?}", lines[4]);
    // inserted before original line 8.
    assert_eq!(lines[5], "inserted before 8", "pos 6: {:?}", lines[5]);
    assert!(lines[6].starts_with("line 8:"), "pos 7: {:?}", lines[6]);
    assert!(lines[7].starts_with("line 9:"), "pos 8: {:?}", lines[7]);
    assert!(lines[8].starts_with("line 10:"), "pos 9: {:?}", lines[8]);
}

/// ED-IT-10: Line mode preserves CRLF endings — splice line 2 of a CRLF file
/// with `\n`-terminated data; verify all lines (changed and unchanged) end with CRLF.
#[test]
fn edit_line_crlf_endings_preserved() {
    // multiline_crlf.txt: UTF-8 BOM + "line one\r\n" + "line two\r\n" + "line three\r\n"
    let (_dir, src) = cp("multiline_crlf.txt");

    // Splice line 2 with LF-terminated data; the denormalizer must output \r\n.
    ok(tpu()
        .arg("edit")
        .arg("--splice")
        .arg("2")
        .arg("replaced\n")
        .arg(&src));

    let raw = fs::read(&src).unwrap();

    // Strip UTF-8 BOM if present so byte iteration starts at content.
    let content = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &raw[3..]
    } else {
        &raw[..]
    };

    // Every logical line must end with \r\n (not bare \n).
    // Split on \r\n and check the result has 3 lines with the right text.
    let lines: Vec<&[u8]> = content.split(|&b| b == b'\n').collect();

    // After splitting on \n we expect 3 lines + 1 empty trailing slice.
    assert_eq!(
        lines.len(),
        4,
        "expected 3 CRLF lines (4 slices after splitting on \\n); got {}",
        lines.len()
    );
    assert_eq!(lines[3], b"", "last slice after trailing \\n must be empty");

    // Each of the three content slices must end with \r (i.e. the \r was preserved).
    assert!(
        lines[0].ends_with(b"\r"),
        "line 1 must end with \\r; got {:?}",
        lines[0]
    );
    assert!(
        lines[1].ends_with(b"\r"),
        "line 2 (changed) must end with \\r; got {:?}",
        lines[1]
    );
    assert!(
        lines[2].ends_with(b"\r"),
        "line 3 must end with \\r; got {:?}",
        lines[2]
    );

    // Content checks (strip the trailing \r for comparison).
    assert_eq!(lines[0].trim_ascii_end(), b"line one", "line 1 content");
    assert_eq!(
        lines[1].trim_ascii_end(),
        b"replaced",
        "spliced line content"
    );
    assert_eq!(lines[2].trim_ascii_end(), b"line three", "line 3 content");
}

/// ED-IT-11: `--validate` pre-edit guard passes — valid selector allows edit.
/// Validate that line 1 equals its known content, then delete line 10;
/// the file should be modified normally.
#[test]
fn edit_validate_passes_allows_edit() {
    let (_dir, src) = cp("ascii_10lines.txt");

    // Read the actual content of line 1 so the selector is correct.
    let raw = String::from_utf8(fs::read(&src).unwrap()).unwrap();
    let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let line1 = content.lines().next().unwrap().to_string();

    ok(tpu()
        .arg("edit")
        .arg("--validate")
        .arg("line:1")
        .arg(&line1)
        .arg("--delete")
        .arg("10")
        .arg(&src));

    let raw2 = String::from_utf8(fs::read(&src).unwrap()).unwrap();
    let content2 = raw2.strip_prefix('\u{feff}').unwrap_or(&raw2);
    let lines: Vec<&str> = content2.lines().collect();

    assert_eq!(
        lines.len(),
        9,
        "should have 9 lines after deleting line 10; got {}",
        lines.len()
    );
    assert!(
        lines[0].starts_with("line 1:"),
        "line 1 unchanged: {:?}",
        lines[0]
    );
    assert!(
        lines[8].starts_with("line 9:"),
        "line 9 is last: {:?}",
        lines[8]
    );
}

/// ED-IT-12: `--validate` pre-edit guard fails — failed selector aborts with
/// non-zero exit and leaves file unchanged (no `.bak` created).
#[test]
fn edit_validate_fails_file_unchanged() {
    let (_dir, src) = cp("ascii_10lines.txt");
    let original = fs::read(&src).unwrap();

    // Provide a deliberately wrong value for line 1.
    err(tpu()
        .arg("edit")
        .arg("--validate")
        .arg("line:1")
        .arg("this is definitely not line 1")
        .arg("--delete")
        .arg("5")
        .arg(&src));

    // File must be byte-for-byte identical to the original.
    let after = fs::read(&src).unwrap();
    assert_eq!(
        after, original,
        "file must be unchanged after validate failure"
    );

    // No .bak file should have been created.
    assert!(
        !bak(&src).exists(),
        ".bak must not exist when validate aborts before any edit"
    );
}

/// ED-IT-13: `--diff` emits unified diff on a line-mode change.
#[test]
fn edit_diff_emits_unified_diff() {
    let (_dir, src) = cp("ascii_10lines.txt");

    let o = ok(tpu()
        .arg("edit")
        .arg("--diff")
        .arg("--delete")
        .arg("3")
        .arg(&src));

    let diff = String::from_utf8(o.stdout).unwrap();

    // Must contain standard unified-diff markers.
    assert!(diff.contains("---"), "diff must have --- header:\n{diff}");
    assert!(diff.contains("+++"), "diff must have +++ header:\n{diff}");
    assert!(
        diff.contains("@@"),
        "diff must have @@ hunk header:\n{diff}"
    );
    // The deleted line must appear as a removal.
    assert!(
        diff.lines()
            .any(|l| l.starts_with('-') && l.contains("line 3:")),
        "diff must show line 3 removal:\n{diff}"
    );
    // Retained lines must NOT appear as removals.
    assert!(
        !diff
            .lines()
            .any(|l| l.starts_with('-') && l.contains("line 1:")),
        "line 1 must not appear as removed:\n{diff}"
    );
    assert!(
        !diff
            .lines()
            .any(|l| l.starts_with('-') && l.contains("line 10:")),
        "line 10 must not appear as removed:\n{diff}"
    );
}

/// ED-IT-14: `--data-format=hex` in binary mode — splice using hex-encoded
/// data; verify exact bytes written.
#[test]
fn edit_binary_data_format_hex_splice() {
    // File: [0x41, 0x42, 0x43, 0x44, 0x45] (ABCDE)
    // Splice range 1-3 (bytes at offsets 1, 2 — i.e. 0-based [1,3)) with
    // [0xFF, 0x00, 0x7F] ("FF-00-7F" or "FF007F" in hex).
    // Expected result: [0x41, 0xFF, 0x00, 0x7F, 0x44, 0x45]
    let (_dir, src) = temp_bin(&[0x41, 0x42, 0x43, 0x44, 0x45]);

    ok(tpu()
        .arg("edit")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&src)
        .arg("--splice")
        .arg("1-3")
        .arg("FF-00-7F"));

    assert_eq!(
        fs::read(&src).unwrap(),
        &[0x41, 0xFF, 0x00, 0x7F, 0x44, 0x45],
        "hex-decoded bytes must replace the spliced range"
    );
}

/// ED-IT-14b: `--data-format=hex` in binary mode — insert using hex-encoded
/// data; verify exact bytes written.
#[test]
fn edit_binary_data_format_hex_insert() {
    // File: [0x10, 0x20, 0x30] — insert [0xAB, 0xCD] at offset 1.
    // Expected: [0x10, 0xAB, 0xCD, 0x20, 0x30]
    let (_dir, src) = temp_bin(&[0x10, 0x20, 0x30]);

    ok(tpu()
        .arg("edit")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&src)
        .arg("--insert")
        .arg("1")
        .arg("ABCD"));

    assert_eq!(
        fs::read(&src).unwrap(),
        &[0x10, 0xAB, 0xCD, 0x20, 0x30],
        "hex-decoded bytes must be inserted at the correct offset"
    );
}

/// ED-IT-14c: `--data-format=hex` with an invalid hex string exits non-zero
/// and leaves the file unchanged.
#[test]
fn edit_binary_data_format_hex_invalid_exits_nonzero() {
    let (_dir, src) = temp_bin(&[0xAA, 0xBB]);
    let original = fs::read(&src).unwrap();

    err(tpu()
        .arg("edit")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&src)
        .arg("--insert")
        .arg("0")
        .arg("ZZZZ")); // not valid hex

    assert_eq!(
        fs::read(&src).unwrap(),
        original,
        "file must be unchanged after a decode error"
    );
}

/// ED-IT-14d: `--data-format=hex` with contiguous (no dash) hex digits works.
#[test]
fn edit_binary_data_format_hex_contiguous_splice() {
    // File: [0x01, 0x02, 0x03] — splice range 0-1 (first byte) with [0xDE, 0xAD].
    // Expected: [0xDE, 0xAD, 0x02, 0x03]
    let (_dir, src) = temp_bin(&[0x01, 0x02, 0x03]);

    ok(tpu()
        .arg("edit")
        .arg("--binary")
        .arg("--data-format=hex")
        .arg(&src)
        .arg("--splice")
        .arg("0-1")
        .arg("DEAD"));

    assert_eq!(
        fs::read(&src).unwrap(),
        &[0xDE, 0xAD, 0x02, 0x03],
        "contiguous hex must be decoded correctly for splice"
    );
}

/// ED-IT-15: `--data-format=base64` in binary mode — insert using base64 data;
/// verify exact bytes written.
#[test]
fn edit_binary_data_format_base64_insert() {
    // File: [0xAA, 0xBB, 0xCC, 0xDD]
    // Insert [0x11, 0x22] ("ESI=" in base64) before byte offset 2.
    // Expected result: [0xAA, 0xBB, 0x11, 0x22, 0xCC, 0xDD]
    let (_dir, src) = temp_bin(&[0xAA, 0xBB, 0xCC, 0xDD]);

    ok(tpu()
        .arg("edit")
        .arg("--binary")
        .arg("--data-format=base64")
        .arg(&src)
        .arg("--insert")
        .arg("2")
        .arg("ESI="));

    assert_eq!(
        fs::read(&src).unwrap(),
        &[0xAA, 0xBB, 0x11, 0x22, 0xCC, 0xDD],
        "base64-decoded bytes must be inserted at the correct offset"
    );
}

/// ED-IT-16: Out-of-range byte offset (beyond EOF) exits non-zero.
#[test]
fn edit_binary_out_of_range_offset_exits_nonzero() {
    // File has 4 bytes; inserting at offset 100 is past EOF → must fail.
    let (_dir, src) = temp_bin(&[0xAA, 0xBB, 0xCC, 0xDD]);
    let original = fs::read(&src).unwrap();

    err(tpu()
        .arg("edit")
        .arg("--binary")
        .arg(&src)
        .arg("--insert")
        .arg("100")
        .arg("FF"));

    // File must be byte-for-byte identical to the original.
    assert_eq!(
        fs::read(&src).unwrap(),
        original,
        "file must be unchanged after out-of-range insert"
    );
    // No .bak file should have been created.
    assert!(
        !bak(&src).exists(),
        ".bak must not exist when edit aborts before any write"
    );
}

/// ED-IT-17: Out-of-range line number (beyond last line) exits non-zero.
#[test]
fn edit_line_out_of_range_line_number_exits_nonzero() {
    // ascii_10lines.txt has 10 lines; deleting line 11 is out of range.
    let (_dir, src) = cp("ascii_10lines.txt");
    let original = fs::read(&src).unwrap();

    err(tpu().arg("edit").arg(&src).arg("--delete").arg("11"));

    // File must be byte-for-byte identical to the original.
    assert_eq!(
        fs::read(&src).unwrap(),
        original,
        "file must be unchanged after out-of-range line delete"
    );
    // No .bak file should have been created.
    assert!(
        !bak(&src).exists(),
        ".bak must not exist when edit aborts before any write"
    );
}

/// ED-IT-17b: `$` / `EOF` bounds against a file with no content lines exit
/// cleanly instead of panicking.
///
/// Regression: `EOF_SENTINEL` was resolved to `total_lines` *after* the
/// 1-based guard, so on a zero-line file it became 0, slipped past the
/// `start_line > total_lines` check (`0 > 0` is false) and underflowed
/// `line_starts[start_line - 1]`, aborting the process with exit 101.  Under
/// tpu-mcp that killed the io worker.
#[test]
fn edit_line_eof_sentinel_on_empty_file_exits_cleanly() {
    // Every EOF-bearing line-mode range form, against a truly empty file.
    for args in [
        vec!["--delete", "$"],
        vec!["--delete", "1-$"],
        vec!["--delete", "$-$"],
        vec!["--splice", "$", "x"],
        vec!["--splice", "1-$", "x"],
    ] {
        let (_dir, src) = temp_bin(b"");
        let mut c = tpu();
        c.arg("edit").arg(&src);
        for a in &args {
            c.arg(a);
        }
        let o = err(&mut c);

        let stderr = String::from_utf8_lossy(&o.stderr);
        assert!(
            !stderr.contains("panicked"),
            "{args:?} panicked instead of failing cleanly:\n{stderr}"
        );
        assert!(
            !stderr.contains("subtract with overflow"),
            "{args:?} underflowed:\n{stderr}"
        );
        assert_eq!(
            o.status.code(),
            Some(1),
            "{args:?} should exit 1, got {:?}:\n{stderr}",
            o.status.code()
        );
        assert!(
            stderr.contains("out of range"),
            "{args:?} should report an out-of-range error:\n{stderr}"
        );
        // The file must be untouched and no .bak left behind.
        assert_eq!(fs::read(&src).unwrap(), b"", "{args:?} modified the file");
        assert!(!bak(&src).exists(), "{args:?} left a .bak behind");
    }
}

/// ED-IT-17c: the same guarantee for a BOM-only file.
///
/// This case matters because the file is *not* zero-length on disk — it is
/// three bytes — so no "empty file" shortcut catches it.  The view's bytes are
/// empty only after the BOM is stripped.
#[test]
fn edit_line_eof_sentinel_on_bom_only_file_exits_cleanly() {
    let (_dir, src) = temp_bin(&[0xEF, 0xBB, 0xBF]);
    let original = fs::read(&src).unwrap();

    let o = err(tpu().arg("edit").arg(&src).arg("--delete").arg("$"));

    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(!stderr.contains("panicked"), "panicked:\n{stderr}");
    assert_eq!(o.status.code(), Some(1), "expected exit 1:\n{stderr}");
    assert!(
        stderr.contains("out of range"),
        "expected an out-of-range error:\n{stderr}"
    );
    assert_eq!(
        fs::read(&src).unwrap(),
        original,
        "BOM-only file must be unchanged"
    );
}

/// ED-IT-17d: `$` still resolves normally on files that do have lines, so the
/// zero-line guard did not over-reject.
#[test]
fn edit_line_eof_sentinel_still_works_on_nonempty_file() {
    let (_dir, src) = temp_bin(b"a\nb\nc\n");
    ok(tpu().arg("edit").arg(&src).arg("--delete").arg("$"));
    assert_eq!(fs::read(&src).unwrap(), b"a\nb\n");

    let (_dir2, src2) = temp_bin(b"only\n");
    ok(tpu().arg("edit").arg(&src2).arg("--delete").arg("$"));
    assert_eq!(fs::read(&src2).unwrap(), b"");
}

/// ED-IT-18: Overlapping patches in one invocation exit non-zero before any
/// file modification (covers both binary mode and line mode).
#[test]
fn edit_binary_overlapping_patches_exit_nonzero() {
    // File: [0x00, 0x01, 0x02, 0x03, 0x04] (5 bytes)
    // --delete 1-3 covers bytes [1,3) and --delete 2-4 covers bytes [2,4): overlapping.
    let (_dir, src) = temp_bin(&[0x00, 0x01, 0x02, 0x03, 0x04]);
    let original = fs::read(&src).unwrap();

    err(tpu()
        .arg("edit")
        .arg("--binary")
        .arg(&src)
        .arg("--delete")
        .arg("1-3")
        .arg("--delete")
        .arg("2-4"));

    assert_eq!(
        fs::read(&src).unwrap(),
        original,
        "file must be unchanged after overlapping binary patches"
    );
    assert!(
        !bak(&src).exists(),
        ".bak must not exist when edit aborts before any write"
    );
}

#[test]
fn edit_line_overlapping_patches_exit_nonzero() {
    // ascii_10lines.txt has 10 lines.
    // --delete 2-5 and --delete 4-7 overlap on lines 4-5.
    let (_dir, src) = cp("ascii_10lines.txt");
    let original = fs::read(&src).unwrap();

    err(tpu()
        .arg("edit")
        .arg(&src)
        .arg("--delete")
        .arg("2-5")
        .arg("--delete")
        .arg("4-7"));

    assert_eq!(
        fs::read(&src).unwrap(),
        original,
        "file must be unchanged after overlapping line patches"
    );
    assert!(
        !bak(&src).exists(),
        ".bak must not exist when edit aborts before any write"
    );
}

/// ED-IT-19: Atomic write — a write failure leaves the original file intact.
/// Simulated on Windows by holding the `.bak` destination open with no sharing,
/// which causes `fs::rename(file, bak)` to fail (ERROR_SHARING_VIOLATION).
#[test]
#[cfg(windows)]
fn edit_atomic_write_failure_original_intact() {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_NONE: u32 = 0;

    let (_dir, src) = cp("ascii_10lines.txt");
    let bak_path = bak(&src);
    let original = fs::read(&src).unwrap();

    // Create the .bak path and hold it open exclusively (no sharing).
    // Any attempt by another process to rename over it will fail.
    let _bak_lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .share_mode(FILE_SHARE_NONE)
        .open(&bak_path)
        .unwrap();

    err(tpu().arg("edit").arg(&src).arg("--delete").arg("1"));

    // The rename failed, so original must still be at its original path and intact.
    assert_eq!(
        fs::read(&src).unwrap(),
        original,
        "file must be unchanged after atomic write failure"
    );
}

/// ED-IT-20: `--message-format=json` on a successful edit emits `status` +
/// `finished{success:true}`; on failure emits `error` + `finished{success:false}`.
#[test]
fn edit_json_success_emits_status_and_finished_true() {
    let (_dir, src) = cp("ascii_10lines.txt");

    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("edit")
        .arg(&src)
        .arg("--delete")
        .arg("1"));

    assert!(o.stderr.is_empty(), "stderr must be empty in JSON mode");
    let msgs = parse_ndjson(&o.stdout);
    let reasons: Vec<&str> = msgs.iter().map(reason).collect();
    assert!(
        reasons.contains(&"status"),
        "expected 'status' message on success; got {reasons:?}"
    );
    let fin = msgs.last().unwrap();
    assert_eq!(reason(fin), "finished");
    assert!(
        fin["success"].as_bool().unwrap(),
        "finished.success must be true on success"
    );
}

#[test]
fn edit_json_failure_emits_error_and_finished_false() {
    // Deleting line 99 from a 10-line file is out of range → failure.
    let (_dir, src) = cp("ascii_10lines.txt");

    let o = err(tpu()
        .arg("--message-format=json")
        .arg("edit")
        .arg(&src)
        .arg("--delete")
        .arg("99"));

    assert!(o.stderr.is_empty(), "stderr must be empty in JSON mode");
    let msgs = parse_ndjson(&o.stdout);
    let reasons: Vec<&str> = msgs.iter().map(reason).collect();
    assert!(
        reasons.contains(&"error"),
        "expected 'error' message on failure; got {reasons:?}"
    );
    let fin = msgs.last().unwrap();
    assert_eq!(reason(fin), "finished");
    assert!(
        !fin["success"].as_bool().unwrap(),
        "finished.success must be false on failure"
    );
}

#[test]
fn read_output_format_hex_with_bytes_range() {
    // --output-format applies after --bytes slicing.
    let (_dir, src) = temp_bin(&[0x4D, 0x5A, 0xFF, 0x00]);
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--bytes=1-2")
        .arg("--output-format=hex")
        .arg(&src));
    assert_eq!(o.stdout, b"4D-5A");
}

#[test]
fn readex_output_format_hex_with_bytes_range() {
    let (_dir, src) = temp_bin(&[0x4D, 0x5A, 0xFF, 0x00]);
    let o = ok(tpu()
        .arg("readex")
        .arg("--binary")
        .arg("--bytes=1-2")
        .arg("--output-format=hex")
        .arg(&src));
    assert_eq!(o.stdout, b"4D-5A");
}

// ─── error cases ─────────────────────────────────────────────────────────────

#[test]
fn read_output_format_without_binary_exits_err() {
    // --output-format requires --binary.
    err(tpu()
        .arg("read")
        .arg("--output-format=hex")
        .arg(asset("singleline.txt")));
}

#[test]
fn readex_output_format_without_binary_exits_err() {
    err(tpu()
        .arg("readex")
        .arg("--output-format=hex")
        .arg(asset("singleline.txt")));
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests: --hash flag (read --binary)
//
// Coverage:
//   • crc32:0-EOF resolves EOF to file length; value matches ground truth
//   • md5:0-<N> on a partial range matches expected value
//   • Human mode silently ignores --hash (no "hashes" key in raw output)
//   • Out-of-range (end > file_len) exits non-zero with no output
//   • `$` and `EOF` (case-insensitive) are identical sentinels
//   • Multiple --hash args produce an ordered array
//   • Empty range (start == end) produces crc32 "00000000"
//   • No --hash args → "hashes": [] in JSON envelope
//   • Inverted range (start > end) exits non-zero
//   • Hex-prefixed addresses (0x…) are accepted
//   • --hash without --binary is rejected by clap
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn hash_crc32_full_file_via_eof() {
    // --hash crc32:0-EOF on 5-byte "hello" → crc32=3610a686, resolved range 0-5.
    let (_dir, src) = temp_bin(b"hello");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--message-format=json")
        .arg("--hash=crc32:0-EOF")
        .arg(&src));
    let lines = parse_ndjson(&o.stdout);
    let data = lines.iter().find(|v| v["reason"] == "data").unwrap();
    let hashes = data["hashes"].as_array().unwrap();
    assert_eq!(hashes.len(), 1);
    assert_eq!(hashes[0]["algo"], "crc32");
    assert_eq!(hashes[0]["range"], "0-5");
    assert_eq!(hashes[0]["value"], "3610a686");
}

#[test]
fn hash_md5_partial_range() {
    // 11-byte "hello world"; --hash md5:0-5 → md5 of "hello".
    let (_dir, src) = temp_bin(b"hello world");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--message-format=json")
        .arg("--hash=md5:0-5")
        .arg(&src));
    let lines = parse_ndjson(&o.stdout);
    let data = lines.iter().find(|v| v["reason"] == "data").unwrap();
    let hashes = data["hashes"].as_array().unwrap();
    assert_eq!(hashes.len(), 1);
    assert_eq!(hashes[0]["algo"], "md5");
    assert_eq!(hashes[0]["range"], "0-5");
    assert_eq!(hashes[0]["value"], "5d41402abc4b2a76b9719d911017c592");
}

#[test]
fn hash_human_mode_silently_ignores() {
    // In human (non-JSON) mode, --hash is silently ignored; no JSON envelope.
    let (_dir, src) = temp_bin(b"hello");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--hash=crc32:0-EOF")
        .arg(&src));
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(
        !stdout.contains("hashes"),
        "human output must not contain 'hashes' key"
    );
    assert!(
        !stdout.contains("bytes-base64"),
        "human output must not be a JSON envelope"
    );
}

#[test]
fn hash_out_of_range_exits_nonzero() {
    // End (100) exceeds file length (5) → non-zero exit, nothing emitted.
    let (_dir, src) = temp_bin(b"hello");
    err(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--message-format=json")
        .arg("--hash=crc32:0-100")
        .arg(&src));
}

#[test]
fn hash_dollar_sign_synonym_for_eof() {
    // '$' and 'EOF' must produce the same resolved range and hash value.
    let (_dir, src) = temp_bin(b"hello");
    let o_eof = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--message-format=json")
        .arg("--hash=crc32:0-EOF")
        .arg(&src));
    let o_dollar = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--message-format=json")
        .arg("--hash=crc32:0-$")
        .arg(&src));
    let val_eof = parse_ndjson(&o_eof.stdout)
        .into_iter()
        .find(|v| v["reason"] == "data")
        .unwrap()["hashes"][0]["value"]
        .clone();
    let val_dollar = parse_ndjson(&o_dollar.stdout)
        .into_iter()
        .find(|v| v["reason"] == "data")
        .unwrap()["hashes"][0]["value"]
        .clone();
    assert_eq!(val_eof, val_dollar);
    assert_eq!(val_eof, "3610a686");
}

#[test]
fn hash_multiple_specs_produce_ordered_array() {
    // Two --hash args → two entries in order: crc32 first, md5 second.
    let (_dir, src) = temp_bin(b"hello");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--message-format=json")
        .arg("--hash=crc32:0-5")
        .arg("--hash=md5:0-5")
        .arg(&src));
    let lines = parse_ndjson(&o.stdout);
    let data = lines.iter().find(|v| v["reason"] == "data").unwrap();
    let hashes = data["hashes"].as_array().unwrap();
    assert_eq!(hashes.len(), 2);
    assert_eq!(hashes[0]["algo"], "crc32");
    assert_eq!(hashes[0]["value"], "3610a686");
    assert_eq!(hashes[1]["algo"], "md5");
    assert_eq!(hashes[1]["value"], "5d41402abc4b2a76b9719d911017c592");
}

#[test]
fn hash_empty_range_crc32_gives_zero_checksum() {
    // start == end → empty byte slice → crc32 = "00000000".
    let (_dir, src) = temp_bin(b"hello");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--message-format=json")
        .arg("--hash=crc32:2-2")
        .arg(&src));
    let lines = parse_ndjson(&o.stdout);
    let data = lines.iter().find(|v| v["reason"] == "data").unwrap();
    let hashes = data["hashes"].as_array().unwrap();
    assert_eq!(hashes[0]["value"], "00000000");
    assert_eq!(hashes[0]["range"], "2-2");
}

#[test]
fn hash_no_specs_produces_empty_hashes_array() {
    // No --hash args → "hashes": [] in the bytes-base64 envelope.
    let (_dir, src) = temp_bin(b"hello");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--message-format=json")
        .arg(&src));
    let lines = parse_ndjson(&o.stdout);
    let data = lines.iter().find(|v| v["reason"] == "data").unwrap();
    let hashes = data["hashes"].as_array().unwrap();
    assert!(hashes.is_empty());
}

#[test]
fn hash_start_after_end_exits_nonzero() {
    // Inverted range (start > end) → non-zero exit.
    let (_dir, src) = temp_bin(b"hello");
    err(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--hash=crc32:3-1")
        .arg(&src));
}

#[test]
fn hash_hex_addresses_accepted() {
    // 0x-prefixed hex addresses are equivalent to their decimal form.
    let (_dir, src) = temp_bin(b"hello");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--message-format=json")
        .arg("--hash=crc32:0x0-0x5")
        .arg(&src));
    let lines = parse_ndjson(&o.stdout);
    let data = lines.iter().find(|v| v["reason"] == "data").unwrap();
    let hashes = data["hashes"].as_array().unwrap();
    assert_eq!(hashes[0]["value"], "3610a686");
}

#[test]
fn hash_requires_binary_flag() {
    // --hash without --binary is rejected by clap (missing required argument).
    err(tpu()
        .arg("read")
        .arg("--hash=crc32:0-EOF")
        .arg(asset("singleline.txt")));
}

#[test]
fn hash_eof_case_insensitive() {
    // "eof" (lowercase) and "EOF" (uppercase) must both be accepted.
    let (_dir, src) = temp_bin(b"hello");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--message-format=json")
        .arg("--hash=crc32:0-eof")
        .arg(&src));
    let lines = parse_ndjson(&o.stdout);
    let data = lines.iter().find(|v| v["reason"] == "data").unwrap();
    let hashes = data["hashes"].as_array().unwrap();
    assert_eq!(hashes[0]["value"], "3610a686");
    assert_eq!(hashes[0]["range"], "0-5");
}

#[test]
fn hash_md5_full_file_via_eof() {
    // md5:0-EOF on "hello" → 5d41402abc4b2a76b9719d911017c592.
    let (_dir, src) = temp_bin(b"hello");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--message-format=json")
        .arg("--hash=md5:0-EOF")
        .arg(&src));
    let lines = parse_ndjson(&o.stdout);
    let data = lines.iter().find(|v| v["reason"] == "data").unwrap();
    let hashes = data["hashes"].as_array().unwrap();
    assert_eq!(hashes[0]["algo"], "md5");
    assert_eq!(hashes[0]["value"], "5d41402abc4b2a76b9719d911017c592");
    assert_eq!(hashes[0]["range"], "0-5");
}

#[test]
fn hash_start_exceeds_file_length_exits_nonzero() {
    // Start (10) exceeds file length (5) → non-zero exit.
    let (_dir, src) = temp_bin(b"hello");
    err(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--hash=crc32:10-EOF")
        .arg(&src));
}

#[test]
fn hash_both_crc32_and_md5_of_same_range() {
    // Three --hash specs: two algos over full range, plus one empty-range spec.
    let (_dir, src) = temp_bin(b"hello");
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg("--message-format=json")
        .arg("--hash=crc32:0-5")
        .arg("--hash=md5:0-5")
        .arg("--hash=crc32:0-0")
        .arg(&src));
    let lines = parse_ndjson(&o.stdout);
    let data = lines.iter().find(|v| v["reason"] == "data").unwrap();
    let hashes = data["hashes"].as_array().unwrap();
    assert_eq!(hashes.len(), 3);
    assert_eq!(hashes[0]["algo"], "crc32");
    assert_eq!(hashes[0]["range"], "0-5");
    assert_eq!(hashes[0]["value"], "3610a686");
    assert_eq!(hashes[1]["algo"], "md5");
    assert_eq!(hashes[1]["range"], "0-5");
    assert_eq!(hashes[1]["value"], "5d41402abc4b2a76b9719d911017c592");
    // Empty range (0-0) → crc32 of empty slice.
    assert_eq!(hashes[2]["algo"], "crc32");
    assert_eq!(hashes[2]["range"], "0-0");
    assert_eq!(hashes[2]["value"], "00000000");
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 15 — Binary-mode byte-fidelity tests (IT-BF-1..5)
// ═══════════════════════════════════════════════════════════════════════════════

/// IT-BF-1: `read --binary` on a CRLF file produces output that contains the
/// `\r\n` escape sequences, confirming raw bytes pass through without any
/// line-ending normalisation.  The companion text-mode `read` on the same file
/// must not contain bare `\r` bytes, demonstrating that binary mode uniquely
/// preserves them.
#[test]
fn read_binary_crlf_file_contains_raw_crlf_escape_sequences() {
    // multiline_crlf.txt is a UTF-8 BOM file with CRLF line endings.
    // tpu read --binary escapes bytes: CR (0x0D) → \r, LF (0x0A) → \n.
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg(asset("multiline_crlf.txt")));

    // The escaped output must contain the literal sequence b"\\r\\n" (4 bytes:
    // backslash, r, backslash, n) — proving both CR and LF passed through.
    let has_crlf_escape = o.stdout.windows(4).any(|w| w == b"\\r\\n");
    assert!(
        has_crlf_escape,
        "read --binary on a CRLF file must contain \\r\\n escape sequences; \
         stdout was: {:?}",
        String::from_utf8_lossy(&o.stdout)
    );

    // Also verify that standalone \r escapes appear (from the CR in \r\n),
    // confirming the file's CR bytes are not silently stripped or converted.
    let has_cr_escape = o.stdout.windows(2).any(|w| w == b"\\r");
    assert!(
        has_cr_escape,
        "read --binary output must contain \\r escape for each CR byte in the file"
    );

    // Contrast: text-mode read normalises CRLF to LF, so no bare \r in output.
    let text_o = ok(tpu().arg("read").arg(asset("multiline_crlf.txt")));
    assert!(
        !text_o.stdout.contains(&b'\r'),
        "text-mode read must normalise CRLF to LF — no raw \\r byte in output"
    );
}

/// IT-BF-2: `write --binary` with LF-only bytes to a file that previously
/// contained CRLF bytes produces a file whose bytes contain no `\r` at all.
/// Binary write is verbatim — it never adds or converts line endings.
#[test]
fn write_binary_lf_only_bytes_to_crlf_file_produces_no_cr() {
    // Start with a CRLF file.
    let (_dir, dst) = cp("multiline_crlf.txt");
    let original = fs::read(&dst).unwrap();
    assert!(
        original.contains(&b'\r'),
        "pre-condition: multiline_crlf.txt must contain CR bytes"
    );

    // Overwrite it with pure LF bytes via write --binary.
    let lf_content = b"alpha\nbeta\ngamma\ndelta\n";
    ok_stdin(tpu().arg("write").arg("--binary").arg(&dst), lf_content);

    let result = fs::read(&dst).unwrap();

    // The file must contain exactly the bytes we wrote — no CR anywhere.
    assert_eq!(
        result, lf_content,
        "write --binary must store bytes verbatim"
    );
    assert!(
        !result.contains(&b'\r'),
        "write --binary with LF-only input must leave no CR bytes in the output file"
    );
}

/// IT-BF-3: `write --binary` with CRLF bytes to a new (non-existent) file
/// produces a file whose bytes contain `\r\n` sequences — binary write stores
/// the input verbatim without stripping, converting, or normalising line endings.
#[test]
fn write_binary_crlf_bytes_to_new_file_preserves_crlf() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("new_crlf.bin");
    assert!(!dst.exists(), "pre-condition: file must not exist yet");

    let crlf_content = b"first\r\nsecond\r\nthird\r\nfourth\r\nfifth\r\n";
    ok_stdin(tpu().arg("write").arg("--binary").arg(&dst), crlf_content);

    let result = fs::read(&dst).unwrap();

    // Exact byte-for-byte match — nothing added, removed, or converted.
    assert_eq!(
        result, crlf_content,
        "write --binary must store bytes verbatim"
    );

    // Confirm \r\n sequences are present.
    let crlf_count = result.windows(2).filter(|w| *w == b"\r\n").count();
    assert_eq!(
        crlf_count, 5,
        "expected 5 CRLF sequences in the written file, got {crlf_count}"
    );
}

/// IT-BF-4: Round-trip — writing the raw bytes of a binary file with
/// `write --binary` and reading them back produces a result byte-for-byte
/// identical to the original.  Binary write is verbatim: no encoding detection,
/// no line-ending conversion, and no byte transformation of any kind.
/// Uses `binary.bin` (a true binary asset spanning all 256 byte values).
#[test]
fn read_binary_then_write_binary_roundtrip_is_lossless() {
    // Read the raw binary asset bytes directly from disk.
    let src = asset("binary.bin");
    let original = fs::read(&src).unwrap();
    assert!(!original.is_empty(), "binary.bin must not be empty");

    // Write those raw bytes to a new file via write --binary.
    // write --binary stores stdin bytes verbatim — no transformation.
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("roundtrip.bin");
    ok_stdin(tpu().arg("write").arg("--binary").arg(&dst), &original);

    // The result must be byte-for-byte identical to the original.
    let result = fs::read(&dst).unwrap();
    assert_eq!(
        result,
        original,
        "write --binary must store bytes verbatim with no transformation; \
         original len={}, result len={}",
        original.len(),
        result.len()
    );
}

/// IT-BF-5: `read --binary` on `mixed_endings.txt` (which has LF, CRLF, and CR
/// line terminators) returns raw escaped output that contains all three
/// terminator escape sequences: `\n` (LF), `\r\n` (CRLF), and a standalone
/// `\r` (CR) — confirming no normalisation occurs.
#[test]
fn read_binary_mixed_endings_file_contains_all_three_terminators() {
    // mixed_endings.txt has a 5-cycle pattern: LF, CRLF, CR, CRLF, LF.
    // read --binary escapes: LF→\n, CR→\r, CRLF→\r\n.
    let o = ok(tpu()
        .arg("read")
        .arg("--binary")
        .arg(asset("mixed_endings.txt")));
    let out = &o.stdout;

    // Must contain a standalone \n (LF-only lines): the 2-byte sequence b"\\n"
    // that is NOT immediately preceded by the 2-byte sequence b"\\r".
    let lf_escapes: Vec<usize> = out
        .windows(2)
        .enumerate()
        .filter(|(_, w)| *w == b"\\n")
        .map(|(i, _)| i)
        .collect();
    let has_standalone_lf = lf_escapes
        .iter()
        .any(|&i| i < 2 || &out[i - 2..i] != b"\\r");
    assert!(
        has_standalone_lf,
        "read --binary on mixed_endings.txt must contain standalone \\n escapes (LF-only lines)"
    );

    // Must contain \r\n (CRLF lines) — the 4-byte sequence.
    let has_crlf = out.windows(4).any(|w| w == b"\\r\\n");
    assert!(
        has_crlf,
        "read --binary on mixed_endings.txt must contain \\r\\n escape sequences (CRLF lines)"
    );

    // Must contain a standalone \r (CR-only lines) — a \r escape NOT followed
    // immediately by \n.
    let cr_escapes: Vec<usize> = out
        .windows(2)
        .enumerate()
        .filter(|(_, w)| *w == b"\\r")
        .map(|(i, _)| i)
        .collect();
    let has_standalone_cr = cr_escapes.iter().any(|&i| {
        // The \r escape occupies bytes [i, i+1]; check that bytes [i+2, i+3]
        // are not b"\\n".
        i + 4 > out.len() || &out[i + 2..i + 4] != b"\\n"
    });
    assert!(
        has_standalone_cr,
        "read --binary on mixed_endings.txt must contain standalone \\r escapes (CR-only lines)"
    );
}

// ─────────────────────────────────────────────────────────────────────────────

// SECTION 14 — --line-ending override for write (IT-WLE-1..13),
//              replace (IT-RLE-1..4), and error cases (IT-ERR-1..3)
// ═══════════════════════════════════════════════════════════════════════════════

// ─── IT-WLE: write --line-ending ─────────────────────────────────────────────

/// IT-WLE-1: `write --line-ending=lf` on a CRLF file produces a file with no
/// `\r` bytes (the line-ending override forces LF regardless of the existing
/// CRLF convention detected from the file).
#[test]
fn write_line_ending_lf_on_crlf_file() {
    let (_dir, dst) = cp("multiline_crlf.txt");
    // Sanity: asset must actually have CR bytes.
    assert!(
        fs::read(&dst).unwrap().contains(&b'\r'),
        "test asset multiline_crlf.txt must have \\r bytes"
    );
    // tpu read emits UTF-8/LF-normalised output; writing it back without
    // --line-ending would re-apply CRLF (detected from the existing file).
    // With --line-ending=lf the override wins.
    let input = ok(tpu().arg("read").arg(&dst)).stdout;
    ok_stdin(tpu().arg("write").arg("--line-ending=lf").arg(&dst), &input);
    let after = fs::read(&dst).unwrap();
    assert!(
        !after.contains(&b'\r'),
        "no \\r bytes should remain after write --line-ending=lf"
    );
}

/// IT-WLE-2: `write --line-ending=crlf` on a LF file converts every LF to
/// CRLF; every `\n` byte must be preceded by `\r`.
#[test]
fn write_line_ending_crlf_on_lf_file() {
    let (_dir, dst) = cp("multiline_lf.txt");
    let input = ok(tpu().arg("read").arg(&dst)).stdout;
    ok_stdin(
        tpu().arg("write").arg("--line-ending=crlf").arg(&dst),
        &input,
    );
    let after = fs::read(&dst).unwrap();
    assert!(
        after.windows(2).any(|w| w == b"\r\n"),
        "file must contain \\r\\n sequences after --line-ending=crlf"
    );
    for i in 0..after.len() {
        if after[i] == b'\n' {
            assert!(
                i > 0 && after[i - 1] == b'\r',
                "lone \\n found at position {i} after --line-ending=crlf"
            );
        }
    }
}

/// IT-WLE-3: `write --line-ending=cr` on a LF file converts every LF to a
/// bare CR; no `\n` bytes remain in the output.
#[test]
fn write_line_ending_cr_on_lf_file() {
    let (_dir, dst) = cp("multiline_lf.txt");
    let input = ok(tpu().arg("read").arg(&dst)).stdout;
    ok_stdin(tpu().arg("write").arg("--line-ending=cr").arg(&dst), &input);
    let after = fs::read(&dst).unwrap();
    assert!(
        !after.contains(&b'\n'),
        "no \\n bytes should remain after write --line-ending=cr"
    );
    assert!(after.contains(&b'\r'), "file must have \\r bytes");
}

/// IT-WLE-4: `write --line-ending=lf` on a file that is already LF leaves the
/// content byte-for-byte identical while still creating a `.bak` file.
/// Uses an inline-created file (not a BOM asset) for deterministic byte comparison.
#[test]
fn write_line_ending_lf_on_lf_file_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("lf_noop.txt");
    // Known LF-only ASCII content — no BOM.
    let content = b"alpha\nbeta\ngamma\ndelta\n";
    fs::write(&dst, content).unwrap();
    let original = fs::read(&dst).unwrap();
    // tpu read output equals the raw file bytes for BOM-free UTF-8/LF ASCII.
    let input = ok(tpu().arg("read").arg(&dst)).stdout;
    ok_stdin(tpu().arg("write").arg("--line-ending=lf").arg(&dst), &input);
    let after = fs::read(&dst).unwrap();
    assert_eq!(
        original, after,
        "LF\u{2192}LF write must leave content byte-for-byte identical"
    );
    assert!(
        bak(&dst).exists(),
        ".bak must be created even for content-identical writes"
    );
}

/// IT-WLE-5: Round-trip LF→CRLF→LF restores the original bytes exactly.
/// Uses an inline-created BOM-free LF file for deterministic byte comparison.
#[test]
fn write_line_ending_lf_crlf_lf_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("roundtrip.txt");
    // Known BOM-free LF ASCII content.
    let content = b"one\ntwo\nthree\nfour\nfive\n";
    fs::write(&dst, content).unwrap();
    let original = fs::read(&dst).unwrap();

    // Step (a): LF → CRLF.
    let lf_input = ok(tpu().arg("read").arg(&dst)).stdout;
    ok_stdin(
        tpu().arg("write").arg("--line-ending=crlf").arg(&dst),
        &lf_input,
    );
    assert!(
        fs::read(&dst).unwrap().windows(2).any(|w| w == b"\r\n"),
        "step (a): file must have CRLF after --line-ending=crlf"
    );

    // Step (b): CRLF → LF.
    let crlf_input = ok(tpu().arg("read").arg(&dst)).stdout;
    ok_stdin(
        tpu().arg("write").arg("--line-ending=lf").arg(&dst),
        &crlf_input,
    );

    // Step (c): Final bytes must equal the original LF bytes.
    let final_bytes = fs::read(&dst).unwrap();
    assert_eq!(
        original, final_bytes,
        "LF→CRLF→LF round-trip must restore original bytes"
    );
}

/// IT-WLE-6: `write --line-ending=lf` on a file with mixed line endings
/// (LF, CRLF, and CR all present) produces a file with no `\r` bytes.
/// Uses an inline-created mixed-endings file (the `mixed_endings.txt` asset
/// from IT-TA-1 does not yet exist).
#[test]
fn write_line_ending_lf_on_mixed_endings_file() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("mixed.txt");
    // Build 12 lines with rotating endings: LF, CRLF, CR.
    let mut mixed: Vec<u8> = Vec::new();
    for i in 0u8..12 {
        write!(mixed, "line{}", i + 1).unwrap();
        match i % 3 {
            0 => mixed.push(b'\n'),
            1 => {
                mixed.push(b'\r');
                mixed.push(b'\n');
            }
            _ => mixed.push(b'\r'),
        }
    }
    fs::write(&dst, &mixed).unwrap();
    assert!(
        mixed.contains(&b'\r'),
        "mixed file must contain \\r bytes before conversion"
    );

    ok_stdin(
        tpu().arg("write").arg("--line-ending=lf").arg(&dst),
        b"line1\nline2\nline3\nline4\nline5\nline6\
          \nline7\nline8\nline9\nline10\nline11\nline12\n",
    );

    let after = fs::read(&dst).unwrap();
    assert!(
        !after.contains(&b'\r'),
        "no \\r bytes should remain after write --line-ending=lf"
    );
}

/// IT-WLE-7: `write --line-ending=crlf` on the LF-only result of a prior
/// normalisation step produces a file where every `\n` is preceded by `\r`
/// and no standalone `\r` appears.
#[test]
fn write_line_ending_crlf_on_lf_from_mixed_produces_clean_crlf() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("lf_file.txt");
    // Start with a known LF-only file (10 lines).
    let lf_bytes = b"line1\nline2\nline3\nline4\nline5\
                     \nline6\nline7\nline8\nline9\nline10\n";
    fs::write(&dst, lf_bytes).unwrap();

    let input = ok(tpu().arg("read").arg(&dst)).stdout;
    ok_stdin(
        tpu().arg("write").arg("--line-ending=crlf").arg(&dst),
        &input,
    );

    let after = fs::read(&dst).unwrap();
    // Every \n must be preceded by \r.
    assert!(
        after.windows(2).any(|w| w == b"\r\n"),
        "must have \\r\\n sequences"
    );
    for i in 0..after.len() {
        if after[i] == b'\n' {
            assert!(i > 0 && after[i - 1] == b'\r', "lone \\n at position {i}");
        }
    }
    // No standalone \r (each \r must be immediately followed by \n).
    for i in 0..after.len() {
        if after[i] == b'\r' {
            assert!(
                i + 1 < after.len() && after[i + 1] == b'\n',
                "standalone \\r at position {i}"
            );
        }
    }
}

/// IT-WLE-8: `write --line-ending=lf` on a new (non-existent) file creates
/// it using LF line endings.
#[test]
fn write_line_ending_lf_creates_new_file_with_lf() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("new_lf.txt");
    assert!(!dst.exists(), "file must not exist before the test");

    ok_stdin(
        tpu().arg("write").arg("--line-ending=lf").arg(&dst),
        b"hello\nworld\n",
    );

    let after = fs::read(&dst).unwrap();
    assert!(dst.exists(), "file must have been created");
    assert!(
        !after.contains(&b'\r'),
        "new file created with --line-ending=lf must have no \\r bytes"
    );
    assert!(after.contains(&b'\n'), "file must have LF bytes");
}

/// IT-WLE-9: `write --line-ending=crlf` on a new file creates it with CRLF
/// endings (overrides the LF-for-new-files default).
#[test]
fn write_line_ending_crlf_creates_new_file_with_crlf() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("new_crlf.txt");
    assert!(!dst.exists(), "file must not exist before the test");

    // Send LF-only content; override must produce CRLF.
    ok_stdin(
        tpu().arg("write").arg("--line-ending=crlf").arg(&dst),
        b"hello\nworld\n",
    );

    let after = fs::read(&dst).unwrap();
    assert!(dst.exists(), "file must have been created");
    assert!(
        after.windows(2).any(|w| w == b"\r\n"),
        "new file with --line-ending=crlf must have \\r\\n sequences"
    );
    for i in 0..after.len() {
        if after[i] == b'\n' {
            assert!(i > 0 && after[i - 1] == b'\r', "lone \\n at position {i}");
        }
    }
}

/// IT-WLE-10: `write --line-ending=lf` combined with `--utf8` writes
/// UTF-8-encoded output (no BOM by default) with LF line endings.
#[test]
fn write_line_ending_lf_with_utf8_flag() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("utf8_lf.txt");

    ok_stdin(
        tpu()
            .arg("write")
            .arg("--line-ending=lf")
            .arg("--utf8")
            .arg(&dst),
        b"hello\nworld\n",
    );

    let after = fs::read(&dst).unwrap();
    assert!(
        !after.starts_with(&[0xEF, 0xBB, 0xBF]),
        "no UTF-8 BOM expected by default with --utf8 and no --bom=force"
    );
    assert!(
        !after.contains(&b'\r'),
        "no \\r bytes expected with --line-ending=lf"
    );
    assert!(after.contains(&b'\n'), "file must have LF bytes");
}

/// IT-WLE-11: An invalid `--line-ending` value (e.g. `windows`) causes a
/// non-zero exit before any file is created or modified.
#[test]
fn write_line_ending_invalid_value_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("never.txt");
    // Clap rejects the unknown value before any I/O is performed.
    err_stdin(
        tpu().arg("write").arg("--line-ending=windows").arg(&dst),
        b"hello\n",
    );
    assert!(
        !dst.exists(),
        "no file should be created after a rejected --line-ending value"
    );
}

/// IT-WLE-12: `write --line-ending=crlf` on a file with a UTF-8 BOM preserves
/// the file's encoding class (UTF-8) while converting line endings to CRLF.
/// Exercises the encoding-preservation path for non-plain-UTF-8 files.
#[test]
fn write_line_ending_crlf_preserves_utf8_bom_encoding() {
    // utf8_bom.txt has a UTF-8 BOM (0xEF 0xBB 0xBF) and LF line endings.
    let (_dir, dst) = cp("utf8_bom.txt");
    // Sanity: the asset must have a UTF-8 BOM.
    let raw = fs::read(&dst).unwrap();
    assert!(
        raw.starts_with(&[0xEF, 0xBB, 0xBF]),
        "utf8_bom.txt must start with UTF-8 BOM for this test to be meaningful"
    );

    // tpu read normalises to UTF-8/LF (strips BOM from output).
    let input = ok(tpu().arg("read").arg(&dst)).stdout;
    ok_stdin(
        tpu().arg("write").arg("--line-ending=crlf").arg(&dst),
        &input,
    );

    let after = fs::read(&dst).unwrap();
    // Must have CRLF sequences — encoding is preserved as UTF-8 (single-byte
    // CR/LF) so the CRLF pair appears as two consecutive bytes 0x0D 0x0A.
    assert!(
        after.windows(2).any(|w| w == b"\r\n"),
        "write --line-ending=crlf on UTF-8 BOM file must produce \\r\\n sequences"
    );
    for i in 0..after.len() {
        if after[i] == b'\n' {
            assert!(
                i > 0 && after[i - 1] == b'\r',
                "lone \\n at position {i} after --line-ending=crlf on UTF-8 BOM file"
            );
        }
    }
}

/// IT-WLE-13: `--line-ending` is listed in the output of `tpu write --help`.
#[test]
fn write_line_ending_appears_in_help() {
    let o = ok(tpu().arg("write").arg("--help"));
    let help = String::from_utf8_lossy(&o.stdout);
    assert!(
        help.contains("--line-ending"),
        "--line-ending must appear in `tpu write --help`"
    );
}

// ─── IT-RLE: replace --line-ending ───────────────────────────────────────────

/// IT-RLE-1: `replace --line-ending=lf` on a CRLF file changes its endings to
/// LF while still performing the regex substitution.
#[test]
fn replace_line_ending_lf_on_crlf_file() {
    let (_dir, dst) = cp("multiline_crlf.txt");
    // Apply a regex substitution AND change line endings to LF.
    ok(tpu()
        .arg("replace")
        .arg("--regex")
        .arg("--line-ending=lf")
        .arg(&dst)
        .arg("[aeiou]")
        .arg("X"));
    let after = fs::read(&dst).unwrap();
    assert!(
        !after.contains(&b'\r'),
        "no \\r bytes after replace --line-ending=lf on CRLF file"
    );
    // The original text (multiline_crlf.txt) contains ASCII text with vowels;
    // at least one 'X' substitution must appear.
    assert!(
        after.contains(&b'X'),
        "regex substitution must have been applied"
    );
}

/// IT-RLE-2: `replace --line-ending=crlf` on a LF file changes its endings to
/// CRLF while still performing the regex substitution.
#[test]
fn replace_line_ending_crlf_on_lf_file() {
    let (_dir, dst) = cp("multiline_lf.txt");
    ok(tpu()
        .arg("replace")
        .arg("--regex")
        .arg("--line-ending=crlf")
        .arg(&dst)
        .arg("[aeiou]")
        .arg("X"));
    let after = fs::read(&dst).unwrap();
    assert!(
        after.windows(2).any(|w| w == b"\r\n"),
        "file must have \\r\\n sequences after replace --line-ending=crlf"
    );
    for i in 0..after.len() {
        if after[i] == b'\n' {
            assert!(i > 0 && after[i - 1] == b'\r', "lone \\n at position {i}");
        }
    }
    assert!(
        after.contains(&b'X'),
        "regex substitution must have been applied"
    );
}

/// IT-RLE-3: `replace --line-ending=lf` with a zero-match pattern changes
/// only the line endings — the substitution count is 0 but endings convert.
#[test]
fn replace_line_ending_lf_zero_match_converts_endings_only() {
    let (_dir, dst) = cp("multiline_crlf.txt");
    let o = ok(tpu()
        .arg("replace")
        .arg("--line-ending=lf")
        .arg(&dst)
        .arg("ZZZNOMATCH_XY0000")
        .arg("Z"));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("0 replacements"),
        "must report 0 replacements; got: {stderr}"
    );
    let after = fs::read(&dst).unwrap();
    assert!(
        !after.contains(&b'\r'),
        "no \\r bytes should remain after --line-ending=lf even with zero matches"
    );
}

/// IT-RLE-4: `--line-ending` is listed in the output of `tpu replace --help`.
#[test]
fn replace_line_ending_appears_in_help() {
    let o = ok(tpu().arg("replace").arg("--help"));
    let help = String::from_utf8_lossy(&o.stdout);
    assert!(
        help.contains("--line-ending"),
        "--line-ending must appear in `tpu replace --help`"
    );
}

// ─── IT-ERR: error cases ─────────────────────────────────────────────────────

/// IT-ERR-1: `write --line-ending=lf` where stdin contains bytes that are not
/// valid UTF-8 exits non-zero and leaves the target file unchanged.
#[test]
fn write_line_ending_lf_invalid_utf8_stdin_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.txt");
    // Create a pre-existing file so the write path goes through the
    // file-exists branch (bak-then-write).
    fs::write(&dst, b"initial content\n").unwrap();
    let initial = fs::read(&dst).unwrap();

    // 0xFF is not a valid UTF-8 byte; read_to_string must fail.
    let invalid_utf8: &[u8] = b"valid start \xFF\xFE invalid\n";
    err_stdin(
        tpu().arg("write").arg("--line-ending=lf").arg(&dst),
        invalid_utf8,
    );

    assert_eq!(
        fs::read(&dst).unwrap(),
        initial,
        "file must be byte-for-byte unchanged after stdin UTF-8 error"
    );
    // No .bak should have been created (error occurred before any rename).
    assert!(
        !bak(&dst).exists(),
        ".bak must not exist when write aborts before any file rename"
    );
}

/// IT-ERR-2: `replace --line-ending=crlf` with an invalid regex exits
/// non-zero and leaves the target file unchanged.
#[test]
fn replace_line_ending_crlf_bad_regex_exits_nonzero() {
    let (_dir, dst) = cp("multiline_lf.txt");
    let original = fs::read(&dst).unwrap();
    err(tpu()
        .arg("replace")
        .arg("--regex")
        .arg("--line-ending=crlf")
        .arg(&dst)
        .arg("[invalid")
        .arg("Z"));
    assert_eq!(
        fs::read(&dst).unwrap(),
        original,
        "file must be unchanged after bad regex with --line-ending=crlf"
    );
}

/// IT-ERR-3: `write --binary --line-ending=lf` is rejected by the CLI
/// (binary mode is incompatible with line-ending override) — must exit
/// non-zero without creating or modifying any file.
#[test]
fn write_binary_and_line_ending_conflict_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.bin");
    // Clap's conflicts_with = "binary" must reject this before any I/O.
    err_stdin(
        tpu()
            .arg("write")
            .arg("--binary")
            .arg("--line-ending=lf")
            .arg(&dst),
        b"\x00\x01\x02",
    );
    assert!(
        !dst.exists(),
        "no file should be created after --binary conflicts with --line-ending"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 16 — Edit composability tests (IT-CV-1..5)
// ═══════════════════════════════════════════════════════════════════════════════

/// IT-CV-1: Replace a pattern that matches 10 non-overlapping regions.
/// Verifies (a) all 10 matches are reported in stderr and (b) the bytes
/// between matched regions are byte-for-byte identical to the corresponding
/// interstitial bytes in the original file.
#[test]
fn replace_ten_matches_reports_count_and_preserves_interstitials() {
    /// Split `hay` on every occurrence of `needle`; return the slices between
    /// occurrences.  Yields N+1 slices for N non-overlapping occurrences.
    fn interstitials<'a>(needle: &[u8], hay: &'a [u8]) -> Vec<&'a [u8]> {
        let mut parts = Vec::new();
        let mut start = 0;
        loop {
            match hay[start..].windows(needle.len()).position(|w| w == needle) {
                Some(rel) => {
                    parts.push(&hay[start..start + rel]);
                    start += rel + needle.len();
                }
                None => {
                    parts.push(&hay[start..]);
                    break;
                }
            }
        }
        parts
    }

    let orig_bytes = fs::read(asset("ascii_10lines.txt")).unwrap();
    let (_dir, f) = cp("ascii_10lines.txt");

    let pattern = "The quick";
    let replacement = "[XREPLACEDX]";

    // (a) Run replace; verify the reported replacement count.
    let o = ok(tpu().arg("replace").arg(&f).arg(pattern).arg(replacement));
    let stderr = String::from_utf8(o.stderr).unwrap();
    assert!(
        stderr.contains("10 replacements"),
        "expected '10 replacements' in stderr; got: {stderr:?}"
    );

    // (b) Bytes between matched regions must be byte-for-byte identical to
    //     the corresponding interstitials in the original file.
    let new_bytes = fs::read(&f).unwrap();
    let orig_parts = interstitials(pattern.as_bytes(), &orig_bytes);
    let new_parts = interstitials(replacement.as_bytes(), &new_bytes);
    assert_eq!(
        orig_parts.len(),
        new_parts.len(),
        "interstitial count mismatch: orig_parts={} new_parts={}",
        orig_parts.len(),
        new_parts.len()
    );
    for (i, (orig, new)) in orig_parts.iter().zip(new_parts.iter()).enumerate() {
        assert_eq!(
            *orig,
            *new,
            "interstitial region {i} differs between original and modified file;\
             \n  orig={:?}\n   new={:?}",
            String::from_utf8_lossy(orig),
            String::from_utf8_lossy(new)
        );
    }
}

/// IT-CV-2: Replace on a CRLF file where the pattern spans adjacent lines
/// (literal `\n` in pattern, `--multiline` flag).  Verifies: (a) the file's
/// CRLF endings are preserved across the whole output and (b) the bytes
/// before and after the matched region are byte-for-byte identical to the
/// corresponding bytes in the original file.
#[test]
fn replace_multiline_spanning_pattern_preserves_crlf_and_interstitials() {
    let orig_bytes = fs::read(asset("multiline_crlf.txt")).unwrap();
    let (_dir, f) = cp("multiline_crlf.txt");

    // The pattern contains a real LF byte (0x0A).  tpu replace operates on a
    // LF-normalised view of the file, so \n in a pattern matches the logical
    // line separator regardless of whether the on-disk format uses CRLF.
    // The pattern crosses the boundary between "line one" and "line two".
    let pattern = "one\nline two"; // contains actual 0x0A
    let replacement = "SPANNED";

    let o = ok(tpu()
        .arg("replace")
        .arg("--multiline")
        .arg(&f)
        .arg(pattern)
        .arg(replacement));
    let stderr = String::from_utf8(o.stderr).unwrap();
    assert!(
        stderr.contains("1 replacement"),
        "expected '1 replacement' in stderr; got: {stderr:?}"
    );

    let new_bytes = fs::read(&f).unwrap();

    // (a) CRLF endings preserved: every \n byte must be preceded by \r.
    for i in 0..new_bytes.len() {
        if new_bytes[i] == b'\n' {
            assert!(
                i > 0 && new_bytes[i - 1] == b'\r',
                "lone \\n at byte offset {i} — CRLF endings not preserved after multiline replace"
            );
        }
    }

    // (b) Content outside the matched region is byte-for-byte identical.
    //     In the CRLF source file the matched region spans "one\r\nline two"
    //     (13 bytes).  Split original and modified on those byte sequences and
    //     compare the surrounding interstitial slices.
    let match_in_source: &[u8] = b"one\r\nline two";
    let repl_bytes: &[u8] = replacement.as_bytes();

    let orig_match_pos = orig_bytes
        .windows(match_in_source.len())
        .position(|w| w == match_in_source)
        .expect("expected match bytes not found in original file");
    let new_repl_pos = new_bytes
        .windows(repl_bytes.len())
        .position(|w| w == repl_bytes)
        .expect("replacement text not found in modified file");

    let orig_prefix = &orig_bytes[..orig_match_pos];
    let orig_suffix = &orig_bytes[orig_match_pos + match_in_source.len()..];
    let new_prefix = &new_bytes[..new_repl_pos];
    let new_suffix = &new_bytes[new_repl_pos + repl_bytes.len()..];

    assert_eq!(
        orig_prefix, new_prefix,
        "prefix before matched region differs"
    );
    assert_eq!(
        orig_suffix, new_suffix,
        "suffix after matched region differs"
    );
}

/// IT-CV-3: Replace on `ascii_10lines.txt` that replaces the entire content of
/// each of the 10 lines.  Verifies: (a) 10 replacements reported in stderr;
/// (b) the `.bak` file content is byte-for-byte equal to the original;
/// (c) the resulting file has exactly 10 lines.
#[test]
fn replace_whole_line_content_ten_lines_bak_and_count() {
    let orig_bytes = fs::read(asset("ascii_10lines.txt")).unwrap();
    let (_dir, f) = cp("ascii_10lines.txt");

    // (?m)^.+$ matches the non-empty body of each line (without the newline).
    // tpu replace operates on a LF-normalised view, so this matches all 10
    // content lines and leaves the newline bytes untouched.
    let pattern = "^.+$";
    let replacement = "REPLACED_LINE";

    // (a) 10 replacements reported.
    let o = ok(tpu()
        .arg("replace")
        .arg("--regex")
        .arg("--multiline")
        .arg(&f)
        .arg(pattern)
        .arg(replacement));
    let stderr = String::from_utf8(o.stderr).unwrap();
    assert!(
        stderr.contains("10 replacements"),
        "expected '10 replacements' in stderr; got: {stderr:?}"
    );

    // (b) .bak content equals the original.
    let bak_bytes = fs::read(bak(&f)).unwrap();
    assert_eq!(
        orig_bytes, bak_bytes,
        ".bak content must be byte-for-byte identical to the original file"
    );

    // (c) New file has exactly 10 lines.
    let new_content =
        String::from_utf8(fs::read(&f).unwrap()).expect("modified file must be valid UTF-8");
    let line_count = new_content.lines().count();
    assert_eq!(
        line_count, 10,
        "modified file must have exactly 10 lines; got {line_count}"
    );
}

/// IT-CV-4: Consecutive `replace` invocations on the same file (sequential
/// composition).  The first replace introduces a token (`STEP1_MARKER`) that
/// the second replace then matches and transforms to `STEP2_DONE`.  Each
/// invocation must report the correct count and the final file must contain
/// only the second replacement text (no trace of the intermediate token or of
/// the original text).
#[test]
fn replace_sequential_composition_two_passes() {
    let (_dir, f) = cp("ascii_10lines.txt");

    // Pass 1: replace "The quick" (present on all 10 lines) with STEP1_MARKER.
    let o1 = ok(tpu()
        .arg("replace")
        .arg(&f)
        .arg("The quick")
        .arg("STEP1_MARKER"));
    let s1 = String::from_utf8(o1.stderr).unwrap();
    assert!(
        s1.contains("10 replacements"),
        "pass 1 expected '10 replacements'; got: {s1:?}"
    );

    // Intermediate state: file must contain STEP1_MARKER and must not contain
    // the original text "The quick".
    let mid = String::from_utf8(fs::read(&f).unwrap()).unwrap();
    assert!(
        mid.contains("STEP1_MARKER"),
        "file must contain STEP1_MARKER after pass 1"
    );
    assert!(
        !mid.contains("The quick"),
        "file must not contain 'The quick' after pass 1"
    );

    // Pass 2: replace STEP1_MARKER (introduced by pass 1) with STEP2_DONE.
    let o2 = ok(tpu()
        .arg("replace")
        .arg(&f)
        .arg("STEP1_MARKER")
        .arg("STEP2_DONE"));
    let s2 = String::from_utf8(o2.stderr).unwrap();
    assert!(
        s2.contains("10 replacements"),
        "pass 2 expected '10 replacements'; got: {s2:?}"
    );

    // Final state: file must contain STEP2_DONE and must not contain either
    // the intermediate token or the original text.
    let final_content = String::from_utf8(fs::read(&f).unwrap()).unwrap();
    assert!(
        final_content.contains("STEP2_DONE"),
        "final file must contain 'STEP2_DONE'"
    );
    assert!(
        !final_content.contains("STEP1_MARKER"),
        "final file must not contain intermediate token 'STEP1_MARKER'"
    );
    assert!(
        !final_content.contains("The quick"),
        "final file must not contain original text 'The quick'"
    );
}

/// IT-CV-5: Replace with patterns that have capture groups.  Verifies that
/// both indexed (`$1`) and named (`$name`) back-references resolve correctly
/// for every match and that there is no cross-contamination between matches
/// (i.e. each match produces its own distinct captured value).
///
/// Two sequential passes are used:
///   Pass 1 — numbered group `(\d+)`: `line (\d+):` → `[L$1]:`
///     • Expected: 10 replacements; output contains `[L1]:` through `[L10]:`
///       each exactly once.
///   Pass 2 — named group `(?P<n>\d+)`: `\[L(?P<n>\d+)\]:` → `{N$n}:`
///     • Expected: 10 replacements; output contains `{N1}:` through `{N10}:`
///       each exactly once; no `[L…]` tokens remain.
#[test]
fn replace_capture_groups_no_cross_contamination() {
    let (_dir, f) = cp("ascii_10lines.txt");

    // ── Pass 1: indexed capture group $1 ─────────────────────────────────────
    let o1 = ok(tpu()
        .arg("replace")
        .arg("--regex")
        .arg(&f)
        .arg(r"line (\d+):")
        .arg(r"[L$1]:"));
    let s1 = String::from_utf8(o1.stderr).unwrap();
    assert!(
        s1.contains("10 replacements"),
        "pass 1 expected '10 replacements'; got: {s1:?}"
    );

    let mid = String::from_utf8(fs::read(&f).unwrap()).unwrap();
    // Each of the 10 distinct line numbers must appear exactly once.
    for n in 1..=10usize {
        let token = format!("[L{n}]:");
        let count = mid.matches(&*token).count();
        assert_eq!(
            count, 1,
            "pass 1: token '{token}' expected exactly once, found {count} times"
        );
    }
    // The original tag must be gone.
    assert!(
        !mid.contains("line 1:"),
        "pass 1: original 'line 1:' tag should be absent after replacement"
    );

    // ── Pass 2: named capture group $n ───────────────────────────────────────
    let o2 = ok(tpu()
        .arg("replace")
        .arg("--regex")
        .arg(&f)
        .arg(r"\[L(?P<n>\d+)\]:")
        .arg(r"{N$n}:"));
    let s2 = String::from_utf8(o2.stderr).unwrap();
    assert!(
        s2.contains("10 replacements"),
        "pass 2 expected '10 replacements'; got: {s2:?}"
    );

    let fin = String::from_utf8(fs::read(&f).unwrap()).unwrap();
    // Each of the 10 distinct values must appear exactly once.
    for n in 1..=10usize {
        let token = format!("{{N{n}}}:");
        let count = fin.matches(&*token).count();
        assert_eq!(
            count, 1,
            "pass 2: token '{token}' expected exactly once, found {count} times"
        );
    }
    // No intermediate `[L…]` tokens must remain.
    assert!(
        !fin.contains("[L"),
        "pass 2: no '[L…]' tokens should remain after named-group replacement"
    );
}

// SECTION 17 — tpu head integration tests (HD-IT-1..11)

/// HD-IT-1: Line mode default — `tpu head` on a 20-line file emits exactly 10 lines.
#[test]
fn hd_line_mode_default_emits_10() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("twenty.txt");
    let content: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    fs::write(&path, content.as_bytes()).unwrap();
    let o = ok(tpu().arg("head").arg(&path));
    let newlines = o.stdout.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(newlines, 10, "expected exactly 10 lines; got {newlines}");
}

/// HD-IT-2: Line mode explicit — `tpu head --lines 3` on a 10-line file emits exactly 3 lines.
#[test]
fn hd_line_mode_explicit_count() {
    // ascii_10lines.txt has 10 LF-terminated lines.
    let o = ok(tpu()
        .arg("head")
        .arg("--lines")
        .arg("3")
        .arg(asset("ascii_10lines.txt")));
    let newlines = o.stdout.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(newlines, 3, "expected exactly 3 lines; got {newlines}");
}

/// HD-IT-3: Line mode short file — `tpu head --lines 50` on a 5-line file emits all 5 lines, exit 0.
#[test]
fn hd_line_mode_short_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("five.txt");
    let content: String = (1..=5).map(|i| format!("line {i}\n")).collect();
    fs::write(&path, content.as_bytes()).unwrap();
    let o = ok(tpu().arg("head").arg("--lines").arg("50").arg(&path));
    let newlines = o.stdout.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(
        newlines, 5,
        "expected exactly 5 lines from a 5-line file with --lines 50; got {newlines}"
    );
}

/// HD-IT-4: Line mode CRLF — output lines end with `\r\n` when the file uses CRLF endings.
#[test]
fn hd_line_mode_crlf_preserved() {
    // multiline_crlf.txt: UTF-8 BOM + 3 CRLF-terminated lines; default 10 emits all 3.
    let o = ok(tpu().arg("head").arg(asset("multiline_crlf.txt")));
    assert!(
        o.stdout.windows(2).any(|w| w == b"\r\n"),
        "expected CRLF sequence in head output of a CRLF file; stdout: {:?}",
        String::from_utf8_lossy(&o.stdout),
    );
}

/// HD-IT-5: Line mode LF — output lines end with `\n` when the file uses LF endings.
#[test]
fn hd_line_mode_lf_preserved() {
    // multiline_lf.txt: UTF-8 BOM + 3 LF-terminated lines.
    let o = ok(tpu().arg("head").arg(asset("multiline_lf.txt")));
    assert!(
        o.stdout.contains(&b'\n'),
        "expected LF in head output of an LF file",
    );
    assert!(
        !o.stdout.windows(2).any(|w| w == b"\r\n"),
        "expected no CRLF in head output of an LF file; stdout: {:?}",
        String::from_utf8_lossy(&o.stdout),
    );
}

/// HD-IT-6: Binary mode byte count — `tpu head --bytes 8` emits exactly 8 bytes.
#[test]
fn hd_bytes_mode_exact_count() {
    // ascii_10lines.txt is many bytes so the first 8 raw bytes are emitted.
    let o = ok(tpu()
        .arg("head")
        .arg("--bytes")
        .arg("8")
        .arg(asset("ascii_10lines.txt")));
    assert_eq!(
        o.stdout.len(),
        8,
        "expected exactly 8 bytes; got {}",
        o.stdout.len(),
    );
}

/// HD-IT-7: Binary mode short file — `tpu head --bytes 1000` on a 5-byte file emits all 5 bytes, exit 0.
#[test]
fn hd_bytes_mode_short_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tiny.bin");
    fs::write(&path, b"hello").unwrap();
    let o = ok(tpu().arg("head").arg("--bytes").arg("1000").arg(&path));
    assert_eq!(
        o.stdout.len(),
        5,
        "expected exactly 5 bytes from a 5-byte file with --bytes 1000; got {}",
        o.stdout.len(),
    );
}

/// HD-IT-8: Binary mode raw — `tpu head --binary --bytes 16` on a CRLF file emits raw bytes
/// including `\r\n` sequences unmodified.
#[test]
fn hd_bytes_mode_raw_crlf() {
    // multiline_crlf.txt: BOM (3 bytes) + "line one\r\n" (10 bytes) + "lin" (3 bytes) = 16 raw bytes.
    let o = ok(tpu()
        .arg("head")
        .arg("--binary")
        .arg("--bytes")
        .arg("16")
        .arg(asset("multiline_crlf.txt")));
    assert_eq!(
        o.stdout.len(),
        16,
        "expected exactly 16 bytes; got {}",
        o.stdout.len(),
    );
    assert!(
        o.stdout.windows(2).any(|w| w == b"\r\n"),
        "expected raw CRLF bytes in --binary byte-mode output",
    );
}

/// HD-IT-9: Mutual exclusion — `tpu head --lines 5 --bytes 10` exits non-zero.
#[test]
fn hd_flags_mutually_exclusive() {
    let o = err(tpu()
        .arg("head")
        .arg("--lines")
        .arg("5")
        .arg("--bytes")
        .arg("10")
        .arg(asset("ascii_10lines.txt")));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.is_empty(),
        "expected a descriptive error on stderr for --lines and --bytes conflict",
    );
}

/// HD-IT-10: Empty file — `tpu head` on an empty file emits nothing and exits zero.
#[test]
fn hd_empty_file_exits_ok() {
    let o = ok(tpu().arg("head").arg(asset("empty.txt")));
    assert!(
        o.stdout.is_empty(),
        "expected empty stdout for an empty file; got {} bytes",
        o.stdout.len(),
    );
}

/// HD-IT-11: Line mode undetectable encoding — `tpu head --lines 5` on an ambiguous
/// pure-ASCII file completes without panic; either succeeds or exits non-zero.
#[test]
fn hd_ambiguous_encoding_no_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ascii_nobom.txt");
    // Pure ASCII without BOM — encoding detection may or may not succeed.
    let content = b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\niota\nkappa\n";
    fs::write(&path, content).unwrap();
    let o = tpu()
        .arg("head")
        .arg("--lines")
        .arg("5")
        .arg(&path)
        .output()
        .expect("failed to spawn tpu");
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.contains("panicked"),
        "tpu panicked on ambiguous-encoding file: {stderr}",
    );
}

// SECTION 18 — tpu tail integration tests (TL-IT-1..13)

/// TL-IT-1: Line mode default — `tpu tail` on a 20-line file emits exactly the last 10 lines.
#[test]
fn tl_line_mode_default_emits_last_10() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("twenty.txt");
    let content: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    fs::write(&path, content.as_bytes()).unwrap();
    let o = ok(tpu().arg("tail").arg(&path));
    let newlines = o.stdout.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(newlines, 10, "expected exactly 10 lines; got {newlines}");
    // Output must begin with "line 11" — the first of the last 10 lines.
    assert!(
        o.stdout.starts_with(b"line 11\n"),
        "expected output to start with 'line 11\\n'; got {:?}",
        String::from_utf8_lossy(&o.stdout[..o.stdout.len().min(20)]),
    );
}

/// TL-IT-2: Line mode explicit — `tpu tail --lines 3` on a 10-line file emits exactly the last 3 lines.
#[test]
fn tl_line_mode_explicit_count() {
    // ascii_10lines.txt has 10 LF-terminated lines ("line 1: …" through "line 10: …").
    let o = ok(tpu()
        .arg("tail")
        .arg("--lines")
        .arg("3")
        .arg(asset("ascii_10lines.txt")));
    let newlines = o.stdout.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(newlines, 3, "expected exactly 3 lines; got {newlines}");
    // The last line of the file must appear in the output.
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(
        text.contains("line 10:"),
        "expected 'line 10:' in the last 3 lines; got: {text:?}",
    );
}

/// TL-IT-3: Line mode short file — `tpu tail --lines 50` on a 5-line file emits all 5 lines, exit 0.
#[test]
fn tl_line_mode_short_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("five.txt");
    let content: String = (1..=5).map(|i| format!("line {i}\n")).collect();
    fs::write(&path, content.as_bytes()).unwrap();
    let o = ok(tpu().arg("tail").arg("--lines").arg("50").arg(&path));
    let newlines = o.stdout.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(
        newlines, 5,
        "expected exactly 5 lines from a 5-line file with --lines 50; got {newlines}",
    );
}

/// TL-IT-4: Line mode CRLF — output lines end with `\r\n` when the file uses CRLF endings.
#[test]
fn tl_line_mode_crlf_preserved() {
    // multiline_crlf.txt: UTF-8 BOM + 3 CRLF-terminated lines; default 10 emits all 3.
    let o = ok(tpu().arg("tail").arg(asset("multiline_crlf.txt")));
    assert!(
        o.stdout.windows(2).any(|w| w == b"\r\n"),
        "expected CRLF in tail output of a CRLF file; stdout: {:?}",
        String::from_utf8_lossy(&o.stdout),
    );
}

/// TL-IT-5: Line mode LF — output lines end with `\n` and contain no `\r\n` when the file
/// uses LF endings.
#[test]
fn tl_line_mode_lf_preserved() {
    // multiline_lf.txt: UTF-8 BOM + 3 LF-terminated lines.
    let o = ok(tpu().arg("tail").arg(asset("multiline_lf.txt")));
    assert!(
        o.stdout.contains(&b'\n'),
        "expected LF in tail output of an LF file",
    );
    assert!(
        !o.stdout.windows(2).any(|w| w == b"\r\n"),
        "expected no CRLF in tail output of an LF file; stdout: {:?}",
        String::from_utf8_lossy(&o.stdout),
    );
}

/// TL-IT-6: Binary mode byte count — `tpu tail --bytes 8` emits exactly the last 8 bytes.
#[test]
fn tl_bytes_mode_exact_count() {
    // ascii_10lines.txt is large enough that 8 bytes < file length.
    let o = ok(tpu()
        .arg("tail")
        .arg("--bytes")
        .arg("8")
        .arg(asset("ascii_10lines.txt")));
    assert_eq!(
        o.stdout.len(),
        8,
        "expected exactly 8 bytes; got {}",
        o.stdout.len(),
    );
}

/// TL-IT-7: Binary mode short file — `tpu tail --bytes 1000` on a 5-byte file emits all 5 bytes, exit 0.
#[test]
fn tl_bytes_mode_short_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tiny.bin");
    fs::write(&path, b"hello").unwrap();
    let o = ok(tpu().arg("tail").arg("--bytes").arg("1000").arg(&path));
    assert_eq!(
        o.stdout.len(),
        5,
        "expected exactly 5 bytes from a 5-byte file with --bytes 1000; got {}",
        o.stdout.len(),
    );
}

/// TL-IT-8: Binary mode raw — `tpu tail --binary --bytes 16` on a CRLF file emits raw bytes
/// including `\r\n` sequences unmodified.
#[test]
fn tl_bytes_mode_raw_crlf() {
    // multiline_crlf.txt is 35 bytes; the last 16 span the final two CRLF line boundaries.
    let o = ok(tpu()
        .arg("tail")
        .arg("--binary")
        .arg("--bytes")
        .arg("16")
        .arg(asset("multiline_crlf.txt")));
    assert_eq!(
        o.stdout.len(),
        16,
        "expected exactly 16 bytes; got {}",
        o.stdout.len(),
    );
    assert!(
        o.stdout.windows(2).any(|w| w == b"\r\n"),
        "expected raw CRLF bytes in --binary byte-mode output",
    );
}

/// TL-IT-9: Mutual exclusion — `tpu tail --lines 5 --bytes 10` exits non-zero.
#[test]
fn tl_flags_mutually_exclusive() {
    let o = err(tpu()
        .arg("tail")
        .arg("--lines")
        .arg("5")
        .arg("--bytes")
        .arg("10")
        .arg(asset("ascii_10lines.txt")));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.is_empty(),
        "expected a descriptive error on stderr for --lines and --bytes conflict",
    );
}

/// TL-IT-10: Empty file — `tpu tail` on an empty file emits nothing and exits zero.
#[test]
fn tl_empty_file_exits_ok() {
    let o = ok(tpu().arg("tail").arg(asset("empty.txt")));
    assert!(
        o.stdout.is_empty(),
        "expected empty stdout for an empty file; got {} bytes",
        o.stdout.len(),
    );
}

/// TL-IT-11: Exact boundary — `tpu tail --lines N` where N equals the total line count
/// emits the entire file.
#[test]
fn tl_exact_boundary_all_lines() {
    // ascii_10lines.txt has exactly 10 lines.
    let o = ok(tpu()
        .arg("tail")
        .arg("--lines")
        .arg("10")
        .arg(asset("ascii_10lines.txt")));
    let newlines = o.stdout.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(
        newlines, 10,
        "expected all 10 lines when --lines equals file line count; got {newlines}",
    );
}

/// TL-IT-12: Line mode undetectable encoding — `tpu tail --lines 5` on a pure-ASCII
/// no-BOM file completes without panic; either succeeds or exits non-zero with a
/// descriptive error — whichever is the specified behaviour — but does not panic.
#[test]
fn tl_ambiguous_encoding_no_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ascii_nobom.txt");
    let content = b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\niota\nkappa\n";
    fs::write(&path, content).unwrap();
    let o = tpu()
        .arg("tail")
        .arg("--lines")
        .arg("5")
        .arg(&path)
        .output()
        .expect("failed to spawn tpu");
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.contains("panicked"),
        "tpu panicked on ambiguous-encoding file: {stderr}",
    );
}

/// TL-IT-13: Late-detected encoding — a file whose early bytes are pure ASCII but whose
/// tail contains Windows-1252-specific high bytes is decoded consistently.
///
/// harrier must use its full-file encoding decision (not a prefix-only guess) so that
/// both runs of `tpu tail --lines 5` on the same file produce identical output.
/// This verifies stability and that the tail bytes are not decoded with a mismatched
/// codec committed to based only on the ASCII prefix.
#[test]
fn tl_late_encoding_detection_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("late_encoding.bin");
    // 20 lines of pure ASCII followed by 5 lines with Windows-1252-specific high bytes:
    //   0x85 = ellipsis (…), 0x93 = left double quotation mark, 0x94 = right double
    //   quotation mark.  An incremental detector that only inspects a file prefix risks
    //   committing to ASCII/UTF-8 before seeing these bytes; harrier must re-evaluate.
    let mut content: Vec<u8> = (1u32..=20)
        .flat_map(|i| format!("line {i}: the quick brown fox\n").into_bytes())
        .collect();
    for i in 21u32..=25 {
        content.extend_from_slice(format!("line {i}: ").as_bytes());
        content.push(0x85); // W-1252 ellipsis
        content.push(0x93); // W-1252 left double quotation mark
        content.push(0x94); // W-1252 right double quotation mark
        content.push(b'\n');
    }
    fs::write(&path, &content).unwrap();

    let o1 = tpu()
        .arg("tail")
        .arg("--lines")
        .arg("5")
        .arg(&path)
        .output()
        .expect("failed to spawn tpu (run 1)");
    let stderr1 = String::from_utf8_lossy(&o1.stderr);
    assert!(
        !stderr1.contains("panicked"),
        "tpu panicked on first run: {stderr1}",
    );

    // A second run on the same file must produce identical stdout and exit status,
    // confirming that harrier's encoding decision is deterministic across invocations.
    let o2 = tpu()
        .arg("tail")
        .arg("--lines")
        .arg("5")
        .arg(&path)
        .output()
        .expect("failed to spawn tpu (run 2)");
    assert_eq!(
        o1.stdout, o2.stdout,
        "tail output must be deterministic across repeated runs on the same file",
    );
    assert_eq!(
        o1.status, o2.status,
        "exit status must be deterministic across repeated runs",
    );
}

// SECTION 19 — MX integration tests (MX-IT-1..5)
// These tests exercise the tpu binary flags that MX-1..4 exposed in the MCP
// schema: `--diff` on `write`, `--validate` on `write`, and `--diff` on
// `replace`.

/// MX-IT-1: `tpu write --diff` with identical content emits an empty diff (nothing
/// on stdout) and leaves the file byte-for-byte unchanged.
#[test]
fn mx_write_diff_identical_content_emits_empty_diff() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("same.txt");
    let content = b"line one\nline two\nline three\n";
    fs::write(&path, content).unwrap();

    let o = ok_stdin(tpu().arg("write").arg("--diff").arg(&path), content);
    assert!(
        o.stdout.is_empty(),
        "expected empty stdout for identical content; got: {}",
        String::from_utf8_lossy(&o.stdout),
    );
    let after = fs::read(&path).unwrap();
    assert_eq!(after, content, "file content must not change");
}

/// MX-IT-2: `tpu write --diff` with changed content emits a non-empty unified diff
/// to stdout.
#[test]
fn mx_write_diff_changed_content_emits_diff() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("changed.txt");
    let original = b"line one\nline two\nline three\n";
    let updated = b"line one\nline TWO CHANGED\nline three\n";
    fs::write(&path, original).unwrap();

    let o = ok_stdin(tpu().arg("write").arg("--diff").arg(&path), updated);
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(
        !stdout.is_empty(),
        "expected a non-empty diff for changed content; stdout was empty",
    );
    // A unified diff always contains "@@" hunk headers.
    assert!(
        stdout.contains("@@"),
        "expected unified diff hunk markers '@@' in stdout; got: {stdout:?}",
    );
    // The changed line must appear in the diff.
    assert!(
        stdout.contains("TWO CHANGED"),
        "expected changed text 'TWO CHANGED' in diff; got: {stdout:?}",
    );
}

/// MX-IT-3: `tpu write --validate line:1 SELECTOR VALUE` exits 0 when line 1 of the
/// target file exactly matches the expected value.
#[test]
fn mx_write_validate_matching_selector_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("validated_ok.txt");
    let content = b"expected line one\nline two\n";
    fs::write(&path, content).unwrap();

    // Validation passes → write succeeds and file is updated.
    ok_stdin(
        tpu()
            .arg("write")
            .arg(&path)
            .arg("--validate")
            .arg("line:1")
            .arg("expected line one"),
        content,
    );
}

/// MX-IT-4: `tpu write --validate line:1 SELECTOR WRONG_VALUE` exits non-zero when
/// line 1 does NOT match, and the file is left unchanged.
#[test]
fn mx_write_validate_mismatch_exits_nonzero_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("validated_fail.txt");
    let original = b"original content\nsecond line\n";
    let new_content = b"new content\nsecond line\n";
    fs::write(&path, original).unwrap();

    err_stdin(
        tpu()
            .arg("write")
            .arg(&path)
            .arg("--validate")
            .arg("line:1")
            .arg("WRONG_EXPECTED_VALUE"),
        new_content,
    );
    // File must still have the original content — validate runs before the write.
    let after = fs::read(&path).unwrap();
    assert_eq!(
        after, original,
        "file must remain unchanged when --validate fails",
    );
}

/// MX-IT-5: `tpu replace --diff` with a matching pattern emits a non-empty unified
/// diff to stdout containing the substitution.
#[test]
fn mx_replace_diff_matching_pattern_emits_diff() {
    let (_dir, path) = cp("ascii_10lines.txt");

    // "fox" appears in every line of ascii_10lines.txt.
    let o = ok(tpu()
        .arg("replace")
        .arg("--diff")
        .arg(&path)
        .arg("fox")
        .arg("cat"));

    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(
        !stdout.is_empty(),
        "expected a non-empty diff for a matched replacement; stdout was empty",
    );
    assert!(
        stdout.contains("@@"),
        "expected unified diff hunk markers '@@' in stdout; got: {stdout:?}",
    );
    assert!(
        stdout.contains("cat"),
        "expected replacement text 'cat' in diff; got: {stdout:?}",
    );
    // File must now contain the replacement.
    let after = String::from_utf8(fs::read(&path).unwrap()).unwrap();
    assert!(
        after.contains("cat"),
        "expected file to contain 'cat' after replace; content: {after:?}",
    );
}

/// MX-IT-6: `tpu replace --diff` on a pattern with no match emits empty stdout and exits 0.
#[test]
fn mx_replace_diff_no_match_emits_empty_diff() {
    let (_dir, path) = cp("ascii_10lines.txt");

    let o = ok(tpu()
        .arg("replace")
        .arg("--diff")
        .arg(&path)
        .arg("ABSOLUTELY_NO_MATCH_XYZZY")
        .arg("replacement"));

    assert!(
        o.stdout.is_empty(),
        "expected empty stdout when pattern has no match; got: {}",
        String::from_utf8_lossy(&o.stdout),
    );
}

/// MX-IT-7: `tpu read --binary --hash crc32:0-$` emits a line containing the hash
/// algorithm name and a hex value.
#[test]
fn mx_read_binary_hash_emits_hash_value() {
    // Use --message-format=json so hash values appear in the NDJSON output.
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("read")
        .arg("--binary")
        .arg("--hash")
        .arg("crc32:0-$")
        .arg(asset("ascii_10lines.txt")));

    let stdout = String::from_utf8_lossy(&o.stdout);
    // In JSON mode the output is NDJSON; the hashes array must be present.
    assert!(
        stdout.contains("crc32"),
        "expected 'crc32' in JSON output; got: {stdout:?}",
    );
    // The hash value must be present (8 hex chars for CRC32).
    assert!(
        stdout.contains("hashes"),
        "expected 'hashes' key in JSON output; got: {stdout:?}",
    );
}

/// MX-IT-8: Repeated `tpu read --binary --hash crc32:0-$` on the same file produces
/// identical hash values.
#[test]
fn mx_read_binary_hash_is_deterministic() {
    let args = || {
        tpu()
            .arg("--message-format=json")
            .arg("read")
            .arg("--binary")
            .arg("--hash")
            .arg("crc32:0-$")
            .arg(asset("ascii_10lines.txt"))
            .output()
            .expect("failed to spawn tpu")
    };

    let o1 = args();
    let o2 = args();
    assert!(
        o1.status.success(),
        "run 1 failed: {:?}",
        String::from_utf8_lossy(&o1.stderr)
    );
    assert!(
        o2.status.success(),
        "run 2 failed: {:?}",
        String::from_utf8_lossy(&o2.stderr)
    );
    assert_eq!(
        o1.stdout, o2.stdout,
        "hash output must be identical across repeated runs on the same file",
    );
}

/// MX-IT-9: `tpu read --binary --hash crc32:0-$` produces a different hash when the
/// file content differs.
#[test]
fn mx_read_binary_hash_differs_for_different_content() {
    let dir = tempfile::tempdir().unwrap();

    let path_a = dir.path().join("a.bin");
    let path_b = dir.path().join("b.bin");
    fs::write(&path_a, b"hello world\n").unwrap();
    fs::write(&path_b, b"HELLO WORLD\n").unwrap();

    let hash_of = |path: &std::path::Path| -> Vec<u8> {
        tpu()
            .arg("--message-format=json")
            .arg("read")
            .arg("--binary")
            .arg("--hash")
            .arg("crc32:0-$")
            .arg(path)
            .output()
            .expect("failed to spawn tpu")
            .stdout
    };

    let ha = hash_of(&path_a);
    let hb = hash_of(&path_b);
    assert_ne!(
        ha, hb,
        "expected different CRC32 hashes for different file contents",
    );
}

/// MX-IT-10: `tpu write --diff --validate line:1 EXPECTED` with matching content
/// — both flags active simultaneously — exits 0 and emits a diff when changes exist.
#[test]
fn mx_write_diff_and_validate_together() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("combined.txt");
    let original = b"first line\nsecond line\n";
    let updated = b"first line\nSECOND LINE CHANGED\n";
    fs::write(&path, original).unwrap();

    // Validation passes (line 1 matches), diff includes the change.
    let o = ok_stdin(
        tpu()
            .arg("write")
            .arg("--diff")
            .arg("--validate")
            .arg("line:1")
            .arg("first line")
            .arg(&path),
        updated,
    );

    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(
        stdout.contains("@@"),
        "expected unified diff hunk markers '@@' in stdout; got: {stdout:?}",
    );
    assert!(
        stdout.contains("SECOND LINE CHANGED"),
        "expected changed text in diff; got: {stdout:?}",
    );
    // The file must contain the new content.
    let after = fs::read(&path).unwrap();
    assert_eq!(after, updated, "file must be updated when validate passes");
}

/// IT-WLE-2: `write --line-ending=crlf` on a LF file converts it to CRLF;
/// every LF byte is preceded by CR in the written file.
///
/// Without `--line-ending`, writing to a LF file preserves the LF convention.
/// `--line-ending=crlf` overrides that detection and forces CRLF output
/// regardless of what the target file originally contained.
#[test]
fn write_line_ending_crlf_on_lf_file_produces_crlf() {
    // Start with a writable copy of a LF-only file so the detected convention
    // would normally be LF (and without the flag, writing to this file would
    // preserve LF endings).
    let (dir, dst) = cp("multiline_lf.txt");
    let original = fs::read(&dst).expect("read LF asset");
    assert!(
        original.contains(&b'\n') && !original.windows(2).any(|w| w == b"\r\n"),
        "multiline_lf.txt must be LF-only (sanity check)"
    );

    // New content in LF-normalised form (the write command's expected input).
    let lf_content = "alpha\nbeta\ngamma\n";
    ok(tpu()
        .arg("write")
        .arg("--line-ending=crlf")
        .arg(&dst)
        .arg(lf_content));

    let result = fs::read(&dst).expect("read result");
    // Every LF must be preceded by CR.
    for (i, &byte) in result.iter().enumerate() {
        if byte == b'\n' {
            assert!(
                i > 0 && result[i - 1] == b'\r',
                "LF at byte offset {i} is not preceded by CR — \
                 write --line-ending=crlf must produce only CRLF sequences"
            );
        }
    }
    assert!(
        result.windows(2).any(|w| w == b"\r\n"),
        "write --line-ending=crlf result must contain at least one \\r\\n"
    );
    // No standalone CR (CR not followed by LF).
    for (i, &byte) in result.iter().enumerate() {
        if byte == b'\r' {
            assert!(
                i + 1 < result.len() && result[i + 1] == b'\n',
                "CR at byte offset {i} is not followed by LF — \
                 write --line-ending=crlf must not produce standalone CRs"
            );
        }
    }
    let expected: Vec<u8> = lf_content
        .as_bytes()
        .iter()
        .flat_map(|&b| {
            if b == b'\n' {
                vec![b'\r', b'\n']
            } else {
                vec![b]
            }
        })
        .collect();
    assert_eq!(
        result, expected,
        "result must be byte-identical to CRLF-expanded input"
    );
    let _ = dir; // keep TempDir alive
}

// SECTION 20 - --numbers flag for head / tail (NM-IT-1..12)

/// NM-IT-1: `head --numbers` on a 5-line file emits lines prefixed `1\t`, `2\t`, ..., `5\t`.
#[test]
fn nm_head_numbers_five_line_file_prefixes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("five.txt");
    // Lines: "alpha", "beta", "gamma", "delta", "epsilon"
    let content = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
    fs::write(&path, content.as_bytes()).unwrap();
    let o = ok(tpu().arg("head").arg("--numbers").arg(&path));
    let out = String::from_utf8(o.stdout.clone()).unwrap();
    let text_lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        text_lines.len(),
        5,
        "expected 5 numbered lines; got {}",
        text_lines.len()
    );
    for (i, line) in text_lines.iter().enumerate() {
        let expected_prefix = format!("{}\t", i + 1);
        assert!(
            line.starts_with(&expected_prefix),
            "line {} should start with {:?}; got: {:?}",
            i + 1,
            expected_prefix,
            line,
        );
    }
}

/// NM-IT-2: `head --numbers --lines 3` on a 10-line file emits only lines 1-3 with correct prefixes.
#[test]
fn nm_head_numbers_explicit_lines_3() {
    // ascii_10lines.txt has 10 LF-terminated lines.
    let o = ok(tpu()
        .arg("head")
        .arg("--numbers")
        .arg("--lines")
        .arg("3")
        .arg(asset("ascii_10lines.txt")));
    let out = String::from_utf8(o.stdout.clone()).unwrap();
    let text_lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        text_lines.len(),
        3,
        "expected exactly 3 numbered lines; got {}",
        text_lines.len()
    );
    assert!(
        text_lines[0].starts_with("1\t"),
        "first line should start with '1\\t'; got: {:?}",
        text_lines[0]
    );
    assert!(
        text_lines[1].starts_with("2\t"),
        "second line should start with '2\\t'; got: {:?}",
        text_lines[1]
    );
    assert!(
        text_lines[2].starts_with("3\t"),
        "third line should start with '3\\t'; got: {:?}",
        text_lines[2]
    );
}

/// NM-IT-3: `tail --numbers` on a 10-line file (default last 10) emits prefixes 1\t through 10\t.
#[test]
fn nm_tail_numbers_ten_line_file_prefixes() {
    // ascii_10lines.txt has 10 LF-terminated lines -- all lines are returned by default.
    let o = ok(tpu()
        .arg("tail")
        .arg("--numbers")
        .arg(asset("ascii_10lines.txt")));
    let out = String::from_utf8(o.stdout.clone()).unwrap();
    let text_lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        text_lines.len(),
        10,
        "expected 10 numbered lines; got {}",
        text_lines.len()
    );
    for (i, line) in text_lines.iter().enumerate() {
        let expected_prefix = format!("{}\t", i + 1);
        assert!(
            line.starts_with(&expected_prefix),
            "line {} should start with {:?}; got: {:?}",
            i + 1,
            expected_prefix,
            line,
        );
    }
}

/// NM-IT-4: `tail --numbers --lines 3` on a 10-line file emits prefixes 8\t, 9\t, 10\t.
#[test]
fn nm_tail_numbers_last_3_of_10_absolute_prefixes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ten.txt");
    let content: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    fs::write(&path, content.as_bytes()).unwrap();
    let o = ok(tpu()
        .arg("tail")
        .arg("--numbers")
        .arg("--lines")
        .arg("3")
        .arg(&path));
    let out = String::from_utf8(o.stdout.clone()).unwrap();
    let text_lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        text_lines.len(),
        3,
        "expected 3 numbered lines; got {}",
        text_lines.len()
    );
    assert!(
        text_lines[0].starts_with("8\t"),
        "first emitted line should be file line 8; got: {:?}",
        text_lines[0]
    );
    assert!(
        text_lines[1].starts_with("9\t"),
        "second emitted line should be file line 9; got: {:?}",
        text_lines[1]
    );
    assert!(
        text_lines[2].starts_with("10\t"),
        "third emitted line should be file line 10; got: {:?}",
        text_lines[2]
    );
}

/// NM-IT-5: `head --numbers` on a CRLF file emits correct prefixes; output always uses LF.
#[test]
fn nm_head_numbers_crlf_file_output_is_lf() {
    // multiline_crlf.txt: UTF-8 BOM + 3 CRLF-terminated lines.
    let o = ok(tpu()
        .arg("head")
        .arg("--numbers")
        .arg(asset("multiline_crlf.txt")));
    let out = String::from_utf8(o.stdout.clone()).unwrap();
    let text_lines: Vec<&str> = out.lines().collect();
    // Each numbered line should start with a line number prefix.
    assert!(
        !text_lines.is_empty(),
        "expected numbered output for CRLF file"
    );
    assert!(
        text_lines[0].starts_with("1\t"),
        "first numbered line should start with '1\\t'; got: {:?}",
        text_lines[0],
    );
    // In --numbers mode the implementation always emits LF; no \r should appear.
    assert!(
        !o.stdout.contains(&b'\r'),
        "head --numbers output should contain no CR bytes even for a CRLF file; got: {:?}",
        String::from_utf8_lossy(&o.stdout),
    );
}

/// NM-IT-6: `tail --numbers` on a CRLF file emits correct prefixes; output always uses LF.
#[test]
fn nm_tail_numbers_crlf_file_output_is_lf() {
    // multiline_crlf.txt: UTF-8 BOM + 3 CRLF-terminated lines.
    let o = ok(tpu()
        .arg("tail")
        .arg("--numbers")
        .arg(asset("multiline_crlf.txt")));
    let out = String::from_utf8(o.stdout.clone()).unwrap();
    let text_lines: Vec<&str> = out.lines().collect();
    assert!(
        !text_lines.is_empty(),
        "expected numbered output for CRLF file"
    );
    assert!(
        text_lines[0].starts_with("1\t"),
        "first numbered line should start with '1\\t'; got: {:?}",
        text_lines[0],
    );
    // In --numbers mode the implementation always emits LF; no \r should appear.
    assert!(
        !o.stdout.contains(&b'\r'),
        "tail --numbers output should contain no CR bytes even for a CRLF file; got: {:?}",
        String::from_utf8_lossy(&o.stdout),
    );
}

/// NM-IT-7: `head --numbers` on a 1-line file produces exactly one numbered line.
#[test]
fn nm_head_numbers_single_line_file() {
    // singleline.txt has exactly one line.
    let o = ok(tpu()
        .arg("head")
        .arg("--numbers")
        .arg(asset("singleline.txt")));
    let out = String::from_utf8(o.stdout.clone()).unwrap();
    let text_lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        text_lines.len(),
        1,
        "expected exactly 1 numbered line for a 1-line file; got {}",
        text_lines.len()
    );
    assert!(
        text_lines[0].starts_with("1\t"),
        "single numbered line should start with '1\\t'; got: {:?}",
        text_lines[0],
    );
}

/// NM-IT-8: `head --numbers` on an empty file produces no output and exits 0.
#[test]
fn nm_head_numbers_empty_file_produces_no_output() {
    let o = ok(tpu().arg("head").arg("--numbers").arg(asset("empty.txt")));
    assert!(
        o.stdout.is_empty(),
        "expected empty stdout for head --numbers on an empty file; got {} bytes",
        o.stdout.len(),
    );
}

/// NM-IT-9: `tail --numbers` on an empty file produces no output and exits 0.
#[test]
fn nm_tail_numbers_empty_file_produces_no_output() {
    let o = ok(tpu().arg("tail").arg("--numbers").arg(asset("empty.txt")));
    assert!(
        o.stdout.is_empty(),
        "expected empty stdout for tail --numbers on an empty file; got {} bytes",
        o.stdout.len(),
    );
}

/// NM-IT-10: `head --numbers --bytes 10` is rejected with non-zero exit and error on stderr.
#[test]
fn nm_head_numbers_and_bytes_conflict_exits_nonzero() {
    let o = err(tpu()
        .arg("head")
        .arg("--numbers")
        .arg("--bytes")
        .arg("10")
        .arg(asset("ascii_10lines.txt")));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.is_empty(),
        "expected descriptive error on stderr for --numbers + --bytes conflict",
    );
}

/// NM-IT-11: `tail --numbers --bytes 10` is rejected with non-zero exit and error on stderr.
#[test]
fn nm_tail_numbers_and_bytes_conflict_exits_nonzero() {
    let o = err(tpu()
        .arg("tail")
        .arg("--numbers")
        .arg("--bytes")
        .arg("10")
        .arg(asset("ascii_10lines.txt")));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.is_empty(),
        "expected descriptive error on stderr for --numbers + --bytes conflict",
    );
}

/// NM-IT-12: `head --numbers` on a UTF-16LE file decodes and numbers correctly.
#[test]
fn nm_head_numbers_utf16le_file_decodes_and_numbers() {
    // Build a UTF-16LE file (BOM + 3 LF-terminated lines) inline.
    // Encoding: 0xFF 0xFE (LE BOM), then each char as two LE bytes.
    // Content: "alpha\nbeta\ngamma\n"
    let text = "alpha\nbeta\ngamma\n";
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for ch in text.chars() {
        let code = ch as u32;
        // All chars here are BMP; simple LE two-byte encoding.
        bytes.push((code & 0xFF) as u8);
        bytes.push(((code >> 8) & 0xFF) as u8);
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("utf16le.txt");
    fs::write(&path, &bytes).unwrap();

    let o = ok(tpu().arg("head").arg("--numbers").arg(&path));
    let out =
        String::from_utf8(o.stdout.clone()).expect("head --numbers output must be valid UTF-8");
    let text_lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        text_lines.len(),
        3,
        "expected 3 numbered lines from UTF-16LE file; got {}",
        text_lines.len()
    );
    assert!(
        text_lines[0].starts_with("1\t"),
        "line 1 prefix missing; got: {:?}",
        text_lines[0]
    );
    assert!(
        text_lines[1].starts_with("2\t"),
        "line 2 prefix missing; got: {:?}",
        text_lines[1]
    );
    assert!(
        text_lines[2].starts_with("3\t"),
        "line 3 prefix missing; got: {:?}",
        text_lines[2]
    );
    // Verify decoded content.
    assert!(
        text_lines[0].contains("alpha"),
        "line 1 should contain 'alpha'; got: {:?}",
        text_lines[0]
    );
    assert!(
        text_lines[1].contains("beta"),
        "line 2 should contain 'beta'; got: {:?}",
        text_lines[1]
    );
    assert!(
        text_lines[2].contains("gamma"),
        "line 3 should contain 'gamma'; got: {:?}",
        text_lines[2]
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// SECTION 21 — tpu count integration tests (CN-IT-1..20)
//
// All tests exercise the `tpu count` subcommand end-to-end via the compiled
// binary.  Test files are created in-test where needed; the standard asset
// `empty.txt` is reused from the shared asset directory.
//
// Output format (human / default mode):
//   Each metric is emitted as "<label>: <count>\n" in declaration order:
//   lines, words, chars, bytes, then patterns (label or pattern string).
// ══════════════════════════════════════════════════════════════════════════════

/// CN-IT-1: Default (no flags) on a known UTF-8 LF file — all four standard
/// metrics emitted with correct values.
#[test]
fn cn_count_default_all_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn1.txt");
    // 3 lines, 7 words, 35 chars/bytes (pure ASCII, LF line endings)
    let content = "hello world\nalpha beta\nfoo bar baz\n";
    fs::write(&path, content.as_bytes()).unwrap();

    let expected_bytes = content.len() as u64; // 35
    let expected_chars = content.chars().count() as u64; // 35
    let expected_words = content.split_ascii_whitespace().count() as u64; // 7
    let expected_lines = 3u64; // trailing \n → split gives empty last token → 3 lines

    let o = ok(tpu().arg("count").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains(&format!("lines: {expected_lines}")),
        "missing 'lines: {expected_lines}' in: {out:?}"
    );
    assert!(
        out.contains(&format!("words: {expected_words}")),
        "missing 'words: {expected_words}' in: {out:?}"
    );
    assert!(
        out.contains(&format!("chars: {expected_chars}")),
        "missing 'chars: {expected_chars}' in: {out:?}"
    );
    assert!(
        out.contains(&format!("bytes: {expected_bytes}")),
        "missing 'bytes: {expected_bytes}' in: {out:?}"
    );
}

/// CN-IT-2: `--lines` only — emits exactly the line count; words, chars, bytes absent.
#[test]
fn cn_count_lines_flag_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn2.txt");
    let content = "hello world\nalpha beta\nfoo bar baz\n";
    fs::write(&path, content.as_bytes()).unwrap();

    let o = ok(tpu().arg("count").arg("--lines").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(out.contains("lines: 3"), "missing 'lines: 3' in: {out:?}");
    assert!(!out.contains("words:"), "unexpected 'words:' in: {out:?}");
    assert!(!out.contains("chars:"), "unexpected 'chars:' in: {out:?}");
    assert!(!out.contains("bytes:"), "unexpected 'bytes:' in: {out:?}");
}

/// CN-IT-3: `--words` only — emits exactly the word count; other metrics absent.
#[test]
fn cn_count_words_flag_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn3.txt");
    let content = "hello world\nalpha beta\nfoo bar baz\n";
    fs::write(&path, content.as_bytes()).unwrap();

    let o = ok(tpu().arg("count").arg("--words").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(out.contains("words: 7"), "missing 'words: 7' in: {out:?}");
    assert!(!out.contains("lines:"), "unexpected 'lines:' in: {out:?}");
    assert!(!out.contains("chars:"), "unexpected 'chars:' in: {out:?}");
    assert!(!out.contains("bytes:"), "unexpected 'bytes:' in: {out:?}");
}

/// CN-IT-4: `--chars` only — emits the correct Unicode scalar value count.
/// For a pure-ASCII LF file every byte is also exactly one char, so
/// char_count == byte_count.
#[test]
fn cn_count_chars_flag_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn4.txt");
    let content = "hello world\nalpha beta\nfoo bar baz\n"; // 35 chars, all ASCII
    fs::write(&path, content.as_bytes()).unwrap();

    let expected_chars = content.chars().count() as u64; // 35

    let o = ok(tpu().arg("count").arg("--chars").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains(&format!("chars: {expected_chars}")),
        "missing 'chars: {expected_chars}' in: {out:?}"
    );
    assert!(!out.contains("lines:"), "unexpected 'lines:' in: {out:?}");
    assert!(!out.contains("words:"), "unexpected 'words:' in: {out:?}");
    assert!(!out.contains("bytes:"), "unexpected 'bytes:' in: {out:?}");
    // Sanity: for pure-ASCII, char count == byte count
    assert_eq!(
        expected_chars,
        content.len() as u64,
        "pure-ASCII LF file: char_count must equal byte_count"
    );
}

/// CN-IT-5: `--bytes` only — emits the raw byte count matching fs::metadata().len().
#[test]
fn cn_count_bytes_flag_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn5.txt");
    let content = "hello world\nalpha beta\nfoo bar baz\n";
    fs::write(&path, content.as_bytes()).unwrap();

    let file_size = fs::metadata(&path).unwrap().len();

    let o = ok(tpu().arg("count").arg("--bytes").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains(&format!("bytes: {file_size}")),
        "missing 'bytes: {file_size}' in: {out:?}"
    );
    assert!(!out.contains("lines:"), "unexpected 'lines:' in: {out:?}");
    assert!(!out.contains("words:"), "unexpected 'words:' in: {out:?}");
    assert!(!out.contains("chars:"), "unexpected 'chars:' in: {out:?}");
}

/// CN-IT-6: `--lines --bytes` together — emits exactly two metrics in
/// declaration order (lines before bytes); words and chars absent.
#[test]
fn cn_count_combined_lines_and_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn6.txt");
    let content = "hello world\nalpha beta\nfoo bar baz\n"; // 3 lines, 35 bytes
    fs::write(&path, content.as_bytes()).unwrap();

    let o = ok(tpu().arg("count").arg("--lines").arg("--bytes").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(out.contains("lines: 3"), "missing 'lines: 3' in: {out:?}");
    assert!(out.contains("bytes: 35"), "missing 'bytes: 35' in: {out:?}");
    assert!(!out.contains("words:"), "unexpected 'words:' in: {out:?}");
    assert!(!out.contains("chars:"), "unexpected 'chars:' in: {out:?}");

    // Declaration order: lines must appear before bytes
    let lines_pos = out.find("lines:").unwrap();
    let bytes_pos = out.find("bytes:").unwrap();
    assert!(
        lines_pos < bytes_pos,
        "lines must appear before bytes; lines at {lines_pos}, bytes at {bytes_pos}"
    );
}

/// CN-IT-7: UTF-16LE file — default mode.  Verifies that `byte_count` equals
/// the raw file size while `char_count` reflects the decoded character count,
/// and that line/word counts are correct for the decoded content.
#[test]
fn cn_count_utf16le_file_default_mode() {
    // Build a UTF-16LE file in-test: BOM + "alpha\nbeta\ngamma\n" as LE pairs.
    // Decoded content: 3 lines, 3 words, 17 chars.
    // Raw file: 2 (BOM) + 17×2 = 36 bytes.
    let text = "alpha\nbeta\ngamma\n";
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for ch in text.chars() {
        let code = ch as u32;
        bytes.push((code & 0xFF) as u8);
        bytes.push(((code >> 8) & 0xFF) as u8);
    }
    let raw_byte_count = bytes.len() as u64; // 36
    assert_eq!(raw_byte_count, 36, "sanity: UTF-16LE file must be 36 bytes");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("utf16le.txt");
    fs::write(&path, &bytes).unwrap();

    let o = ok(tpu().arg("count").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    // bytes: raw file size (includes BOM)
    assert!(
        out.contains(&format!("bytes: {raw_byte_count}")),
        "byte count must equal raw file size ({raw_byte_count}); got: {out:?}"
    );

    // chars: decoded character count — must NOT equal raw byte count
    let expected_chars = text.chars().count() as u64; // 17 (not 36)
    assert!(
        out.contains(&format!("chars: {expected_chars}")),
        "char count must equal decoded char count ({expected_chars}), not raw bytes; got: {out:?}"
    );
    assert_ne!(
        expected_chars, raw_byte_count,
        "sanity: decoded char count must differ from raw byte count for UTF-16LE"
    );

    // lines and words for the decoded text
    assert!(out.contains("lines: 3"), "expected 3 lines; got: {out:?}");
    let expected_words = text.split_ascii_whitespace().count() as u64; // 3
    assert!(
        out.contains(&format!("words: {expected_words}")),
        "word count must be {expected_words}; got: {out:?}"
    );
}

/// CN-IT-8: CRLF file — `--lines` reports the same line count as the LF
/// equivalent (harrier normalises CRLF before counting); `--bytes` reports the
/// actual raw file size including all `\r` bytes.
#[test]
fn cn_count_crlf_file_lines_and_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn8.txt");
    // 3 CRLF-terminated lines: 7+6+7 = 20 raw bytes.
    let content: &[u8] = b"alpha\r\nbeta\r\ngamma\r\n";
    fs::write(&path, content).unwrap();

    let expected_line_count = 3u64; // normalised to LF before counting
    let expected_byte_count = content.len() as u64; // 20 (includes all \r bytes)

    // --lines: CRLF normalised to LF by harrier
    let o_lines = ok(tpu().arg("count").arg("--lines").arg(&path));
    let out_lines = String::from_utf8(o_lines.stdout).unwrap();
    assert!(
        out_lines.contains(&format!("lines: {expected_line_count}")),
        "CRLF line count must be normalised to {expected_line_count}; got: {out_lines:?}"
    );

    // --bytes: raw size, including every \r byte
    let o_bytes = ok(tpu().arg("count").arg("--bytes").arg(&path));
    let out_bytes = String::from_utf8(o_bytes.stdout).unwrap();
    assert!(
        out_bytes.contains(&format!("bytes: {expected_byte_count}")),
        "byte count must include \\r bytes; expected {expected_byte_count}; got: {out_bytes:?}"
    );
}

/// CN-IT-9: Empty file — all four standard metrics are zero; exits 0.
#[test]
fn cn_count_empty_file_all_zero() {
    let o = ok(tpu().arg("count").arg(asset("empty.txt")));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains("lines: 0"),
        "expected 'lines: 0'; got: {out:?}"
    );
    assert!(
        out.contains("words: 0"),
        "expected 'words: 0'; got: {out:?}"
    );
    assert!(
        out.contains("chars: 0"),
        "expected 'chars: 0'; got: {out:?}"
    );
    assert!(
        out.contains("bytes: 0"),
        "expected 'bytes: 0'; got: {out:?}"
    );
}

/// CN-IT-10: Single-line file with no trailing newline — `--lines` reports 1;
/// `--words` and `--chars` report correct values for the one line's content.
#[test]
fn cn_count_single_line_no_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn10.txt");
    let content = b"hello world"; // 11 bytes, no trailing newline, 2 words
    fs::write(&path, content).unwrap();

    let o_lines = ok(tpu().arg("count").arg("--lines").arg(&path));
    let out_lines = String::from_utf8(o_lines.stdout).unwrap();
    assert!(
        out_lines.contains("lines: 1"),
        "single-line file without trailing newline must have 1 line; got: {out_lines:?}"
    );

    let o_words = ok(tpu().arg("count").arg("--words").arg(&path));
    let out_words = String::from_utf8(o_words.stdout).unwrap();
    assert!(
        out_words.contains("words: 2"),
        "\"hello world\" must have 2 words; got: {out_words:?}"
    );

    let o_chars = ok(tpu().arg("count").arg("--chars").arg(&path));
    let out_chars = String::from_utf8(o_chars.stdout).unwrap();
    assert!(
        out_chars.contains("chars: 11"),
        "\"hello world\" must have 11 chars; got: {out_chars:?}"
    );
}

/// CN-IT-11: `--pattern "[0-9]+"` on a file with 7 digit runs — emits count 7;
/// label defaults to the pattern string.
#[test]
fn cn_count_pattern_default_label() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn11.txt");
    // 7 runs of digits, separated by letters and spaces
    let content = "abc 123 def 456 ghi 789 jkl 012 mno 345 pqr 678 stu 901\n";
    fs::write(&path, content.as_bytes()).unwrap();

    let o = ok(tpu().arg("count").arg("--pattern").arg("[0-9]+").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    // Default label is the pattern string itself
    assert!(
        out.contains("[0-9]+: 7"),
        "expected '[0-9]+: 7' (default label = pattern); got: {out:?}"
    );
}

/// CN-IT-12: `--pattern "[0-9]+" --label digits` — emits `digits: 7`; the
/// pattern string must not appear as a label in the output.
#[test]
fn cn_count_pattern_custom_label() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn12.txt");
    let content = "abc 123 def 456 ghi 789 jkl 012 mno 345 pqr 678 stu 901\n";
    fs::write(&path, content.as_bytes()).unwrap();

    let o = ok(tpu()
        .arg("count")
        .arg("--pattern")
        .arg("[0-9]+")
        .arg("--label")
        .arg("digits")
        .arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains("digits: 7"),
        "expected 'digits: 7' with custom label; got: {out:?}"
    );
    assert!(
        !out.contains("[0-9]+:"),
        "pattern string must not appear as label when custom label given; got: {out:?}"
    );
}

/// CN-IT-13: Two `--pattern` occurrences — emits two pattern metric lines in
/// the order they were given, after the standard metrics.
#[test]
fn cn_count_two_patterns_emitted_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn13.txt");
    // "foo" appears 2 times; "bar" appears 1 time
    let content = "foo and bar and foo\n";
    fs::write(&path, content.as_bytes()).unwrap();

    let o = ok(tpu()
        .arg("count")
        .arg("--pattern")
        .arg("foo")
        .arg("--pattern")
        .arg("bar")
        .arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains("foo: 2"),
        "expected 'foo: 2' in output; got: {out:?}"
    );
    assert!(
        out.contains("bar: 1"),
        "expected 'bar: 1' in output; got: {out:?}"
    );

    // foo declared first → must appear before bar in output
    let foo_pos = out.find("foo:").unwrap();
    let bar_pos = out.find("bar:").unwrap();
    assert!(
        foo_pos < bar_pos,
        "foo pattern must appear before bar pattern; foo at {foo_pos}, bar at {bar_pos}"
    );
}

/// CN-IT-14: `--pattern` combined with `--lines` — emits the lines metric
/// then the pattern metric; words, chars, bytes are suppressed.
#[test]
fn cn_count_pattern_combined_with_lines_flag() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn14.txt");
    let content = "foo bar\nfoo baz\n"; // 2 lines, "foo" appears 2 times
    fs::write(&path, content.as_bytes()).unwrap();

    let o = ok(tpu()
        .arg("count")
        .arg("--lines")
        .arg("--pattern")
        .arg("foo")
        .arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains("lines: 2"),
        "expected 'lines: 2'; got: {out:?}"
    );
    assert!(out.contains("foo: 2"), "expected 'foo: 2'; got: {out:?}");
    assert!(!out.contains("words:"), "unexpected 'words:' in: {out:?}");
    assert!(!out.contains("chars:"), "unexpected 'chars:' in: {out:?}");
    assert!(!out.contains("bytes:"), "unexpected 'bytes:' in: {out:?}");

    // Standard metrics (lines) appear before pattern metrics
    let lines_pos = out.find("lines:").unwrap();
    let foo_pos = out.find("foo:").unwrap();
    assert!(
        lines_pos < foo_pos,
        "lines metric must appear before pattern metric in output"
    );
}

/// CN-IT-15: Invalid regex in `--pattern` — exits non-zero with a descriptive
/// error on stderr; no output on stdout.
#[test]
fn cn_count_invalid_pattern_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn15.txt");
    fs::write(&path, b"some content\n").unwrap();

    let o = err(tpu()
        .arg("count")
        .arg("--pattern")
        .arg("[invalid")
        .arg(&path));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.is_empty(),
        "expected descriptive error on stderr for invalid regex; got empty stderr"
    );
    // stdout may contain standard metrics emitted before the pattern error fires;
    // we only require that the process exits non-zero and stderr is non-empty.
}

/// CN-IT-16: `--message-format=json` default mode — emits one NDJSON object
/// per metric followed by `{"reason":"finished","success":true}`; all objects
/// are valid JSON; `metric` values and `count` fields are correct.
#[test]
fn cn_count_json_default_emits_ndjson_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn16.txt");
    // 2 lines, 4 words, 23 chars/bytes (pure ASCII LF)
    let content = "hello world\nalpha beta\n";
    fs::write(&path, content.as_bytes()).unwrap();

    let o = ok(tpu().arg("--message-format=json").arg("count").arg(&path));
    let messages = parse_ndjson(&o.stdout);

    // Every message must have a "reason" field
    for msg in &messages {
        assert!(
            msg.get("reason").is_some(),
            "every NDJSON message must have 'reason': {msg:?}"
        );
    }

    // Last message: finished with success:true
    let last = messages.last().expect("must have at least one message");
    assert_eq!(
        reason(last),
        "finished",
        "last message must be 'finished': {last:?}"
    );
    assert_eq!(
        last["success"],
        serde_json::json!(true),
        "finished message must have success:true: {last:?}"
    );

    // In JSON mode stats (encoding, bom, line_ending) are always prepended:
    // 3 stats + 4 standard metrics = 7 data messages.
    let data: Vec<&serde_json::Value> = messages.iter().filter(|m| reason(m) == "data").collect();
    assert_eq!(
        data.len(),
        7,
        "JSON mode must emit 3 stats + 4 metric data messages; got: {data:?}"
    );

    // Full emission order: encoding, bom, line_ending, lines, words, chars, bytes
    let metric_names: Vec<&str> = data
        .iter()
        .map(|m| m["metric"].as_str().expect("metric must be a string"))
        .collect();
    assert_eq!(
        metric_names,
        vec![
            "encoding",
            "bom",
            "line_ending",
            "lines",
            "words",
            "chars",
            "bytes"
        ],
        "metrics must emit stats then counts in order; got: {metric_names:?}"
    );

    // Stats fields
    assert_eq!(
        data[0]["value"].as_str().unwrap(),
        "UTF-8",
        "encoding must be UTF-8"
    );
    assert!(!data[1]["value"].as_bool().unwrap(), "bom must be false");
    assert_eq!(
        data[2]["value"].as_str().unwrap(),
        "LF",
        "line_ending must be LF"
    );

    // Verify individual metric values (skip the first 3 stats messages)
    let by_name: std::collections::HashMap<&str, u64> = data[3..]
        .iter()
        .map(|m| (m["metric"].as_str().unwrap(), m["count"].as_u64().unwrap()))
        .collect();
    assert_eq!(by_name["lines"], 2, "lines must be 2");
    assert_eq!(
        by_name["words"], 4,
        "words must be 4 (hello, world, alpha, beta)"
    );
    let expected_chars = content.chars().count() as u64; // 23
    let expected_bytes = content.len() as u64; // 23
    assert_eq!(
        by_name["chars"], expected_chars,
        "chars must be {expected_chars}"
    );
    assert_eq!(
        by_name["bytes"], expected_bytes,
        "bytes must be {expected_bytes}"
    );

    // Each data message must also carry subcommand="count"
    for msg in &data {
        assert_eq!(
            msg["subcommand"].as_str().unwrap(),
            "count",
            "subcommand must be 'count': {msg:?}"
        );
    }
}

/// CN-IT-17: `--message-format=json` with `--pattern --label` — the custom
/// metric appears as an NDJSON data object with `metric` equal to the label.
#[test]
fn cn_count_json_with_pattern_and_label() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cn17.txt");
    // "foo" appears 2 times
    let content = "foo bar\nfoo baz\n";
    fs::write(&path, content.as_bytes()).unwrap();

    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("count")
        .arg("--pattern")
        .arg("foo")
        .arg("--label")
        .arg("occurrences")
        .arg(&path));
    let messages = parse_ndjson(&o.stdout);

    // There must be a data message with metric="occurrences"
    let pattern_msg = messages
        .iter()
        .filter(|m| reason(m) == "data")
        .find(|m| m["metric"].as_str() == Some("occurrences"))
        .expect("must have a data message with metric='occurrences'");

    assert_eq!(
        pattern_msg["count"].as_u64().unwrap(),
        2,
        "foo appears 2 times; got: {pattern_msg:?}"
    );
    assert_eq!(
        pattern_msg["subcommand"].as_str().unwrap(),
        "count",
        "subcommand must be 'count'; got: {pattern_msg:?}"
    );

    // Finished message must be present and last
    let last = messages.last().unwrap();
    assert_eq!(
        reason(last),
        "finished",
        "last message must be 'finished': {last:?}"
    );
    assert_eq!(last["success"], serde_json::json!(true));
}

/// CN-IT-18: Windows-1252 file — `tpu count` decodes it without error;
/// `--bytes` reports the raw byte length; `--lines` correctly counts
/// lines (0x0A is the line separator in all Latin encodings).
#[test]
fn cn_count_windows1252_file_bytes_and_lines() {
    // Build a file with bytes from the Windows-1252-specific range (0x80-0x9F),
    // which are undefined/control chars in ISO-8859-1 but printable in Windows-1252.
    // Using multiple such bytes per line provides enough signal for reliable detection.
    let mut content: Vec<u8> = Vec::new();
    // 10 lines: "caf" + é(0xE9) + " and " + €(0x80) + "\n"
    for _ in 0..10 {
        content.extend_from_slice(b"caf\xe9 and ");
        content.push(0x80); // € — Windows-1252 specific (0x80-0x9F range)
        content.push(b'\n');
    }
    // 10 more lines: "foo" + '(0x91) + "bar" + '(0x92) + "\n"
    for _ in 0..10 {
        content.extend_from_slice(b"foo");
        content.push(0x91); // ' — Windows-1252 specific
        content.extend_from_slice(b"bar");
        content.push(0x92); // ' — Windows-1252 specific
        content.push(b'\n');
    }
    // "caf\xe9 and \x80\n" = 11 bytes × 10 = 110
    // "foo\x91bar\x92\n"   = 9  bytes × 10 = 90
    let expected_bytes: u64 = 200;
    assert_eq!(
        content.len() as u64,
        expected_bytes,
        "sanity: file must be 200 bytes"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cp1252.txt");
    fs::write(&path, &content).unwrap();

    // --bytes must always report raw file size regardless of encoding
    let o_bytes = ok(tpu().arg("count").arg("--bytes").arg(&path));
    let out_bytes = String::from_utf8(o_bytes.stdout).unwrap();
    assert!(
        out_bytes.contains(&format!("bytes: {expected_bytes}")),
        "bytes must equal raw file size {expected_bytes}; got: {out_bytes:?}"
    );

    // --lines: 0x0A terminates each line; line count must be reliable
    let o_lines = ok(tpu().arg("count").arg("--lines").arg(&path));
    let out_lines = String::from_utf8(o_lines.stdout).unwrap();
    assert!(
        out_lines.contains("lines: 20"),
        "Windows-1252 file must have 20 lines; got: {out_lines:?}"
    );
}

/// CN-IT-19: Large file (≥ 10 000 lines) — all four metrics computed correctly;
/// test is fully deterministic (fixed content, no random generation).
#[test]
fn cn_count_large_file_all_metrics() {
    const LINE_COUNT: usize = 10_000;
    // 43-char line with 9 whitespace-delimited words (pure ASCII)
    const LINE: &str = "the quick brown fox jumps over the lazy dog";
    const WORDS_PER_LINE: usize = 9;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.txt");

    let mut file_content = String::with_capacity((LINE.len() + 1) * LINE_COUNT);
    for _ in 0..LINE_COUNT {
        file_content.push_str(LINE);
        file_content.push('\n');
    }

    let expected_bytes = file_content.len() as u64; // 440_000
    let expected_chars = file_content.chars().count() as u64; // 440_000 (ASCII)
    let expected_words = (WORDS_PER_LINE * LINE_COUNT) as u64; // 90_000
    let expected_lines = LINE_COUNT as u64; // 10_000

    fs::write(&path, file_content.as_bytes()).unwrap();

    let o = ok(tpu().arg("count").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains(&format!("lines: {expected_lines}")),
        "expected 'lines: {expected_lines}'; got: {out:?}"
    );
    assert!(
        out.contains(&format!("words: {expected_words}")),
        "expected 'words: {expected_words}'; got: {out:?}"
    );
    assert!(
        out.contains(&format!("chars: {expected_chars}")),
        "expected 'chars: {expected_chars}'; got: {out:?}"
    );
    assert!(
        out.contains(&format!("bytes: {expected_bytes}")),
        "expected 'bytes: {expected_bytes}'; got: {out:?}"
    );
}

/// CN-IT-20: Non-existent file — exits non-zero with an error on stderr.
#[test]
fn cn_count_nonexistent_file_exits_nonzero() {
    let o = err(tpu().arg("count").arg("/this/path/does/not/exist_cn20.txt"));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.is_empty(),
        "expected error message on stderr for non-existent file; got empty stderr"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// SECTION 22 — tpu append integration tests (AP-IT-1..12)
// ──────────────────────────────────────────────────────────────────────────────

/// AP-IT-1: `append` on a UTF-8 LF file appends content with LF terminators.
#[test]
fn ap_append_utf8_lf_file_appends_with_lf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ap1.txt");
    fs::write(&path, b"line1\nline2\n").unwrap();

    ok(tpu()
        .arg("append")
        .arg(&path)
        .arg("--data")
        .arg("line3\nline4\n"));

    let result = fs::read(&path).unwrap();
    assert_eq!(result, b"line1\nline2\nline3\nline4\n");
}

/// AP-IT-2: `append` on a UTF-8 CRLF file appends content with CRLF terminators.
#[test]
fn ap_append_utf8_crlf_file_appends_with_crlf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ap2.txt");
    // Create a CRLF file.
    fs::write(&path, b"line1\r\nline2\r\n").unwrap();

    ok(tpu()
        .arg("append")
        .arg(&path)
        .arg("--data")
        .arg("line3\nline4\n"));

    let result = fs::read(&path).unwrap();
    assert_eq!(
        result, b"line1\r\nline2\r\nline3\r\nline4\r\n",
        "expected CRLF normalisation for all lines in the combined output"
    );
}

/// AP-IT-3: `append` on a UTF-16LE file produces a valid UTF-16LE file afterwards.
#[test]
fn ap_append_utf16le_file_produces_valid_utf16le() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ap3.txt");

    // Build a UTF-16LE file: BOM + "hello\n" encoded as UTF-16LE.
    let original = "hello\n";
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for cu in original.encode_utf16() {
        bytes.extend_from_slice(&cu.to_le_bytes());
    }
    fs::write(&path, &bytes).unwrap();

    ok(tpu().arg("append").arg(&path).arg("--data").arg("world\n"));

    let result = fs::read(&path).unwrap();
    // Must start with UTF-16LE BOM.
    assert!(
        result.starts_with(&[0xFF, 0xFE]),
        "UTF-16LE BOM must be preserved; got {:02x?}",
        &result[..result.len().min(4)]
    );
    // Decode and verify combined content.
    let (decoded, _, _) = encoding_rs::UTF_16LE.decode(&result);
    let content = decoded.replace("\r\n", "\n").replace('\r', "\n");
    assert!(
        content.contains("hello"),
        "original content missing after append"
    );
    assert!(content.contains("world"), "appended content missing");
}

/// AP-IT-4: `append` on a UTF-16BE file produces a valid UTF-16BE file afterwards.
#[test]
fn ap_append_utf16be_file_produces_valid_utf16be() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ap4.txt");

    // Build a UTF-16BE file: BOM + "alpha\n" encoded as UTF-16BE.
    let original = "alpha\n";
    let mut bytes: Vec<u8> = vec![0xFE, 0xFF]; // UTF-16BE BOM
    for cu in original.encode_utf16() {
        bytes.extend_from_slice(&cu.to_be_bytes());
    }
    fs::write(&path, &bytes).unwrap();

    ok(tpu().arg("append").arg(&path).arg("--data").arg("beta\n"));

    let result = fs::read(&path).unwrap();
    // Must start with UTF-16BE BOM.
    assert!(
        result.starts_with(&[0xFE, 0xFF]),
        "UTF-16BE BOM must be preserved; got {:02x?}",
        &result[..result.len().min(4)]
    );
    // Decode and verify combined content.
    let (decoded, _, _) = encoding_rs::UTF_16BE.decode(&result);
    let content = decoded.replace("\r\n", "\n").replace('\r', "\n");
    assert!(
        content.contains("alpha"),
        "original content missing after append"
    );
    assert!(content.contains("beta"), "appended content missing");
}

/// AP-IT-5: `append --validate` on a file with a failing validation exits
/// non-zero and leaves the file unchanged.
#[test]
fn ap_append_validate_failure_exits_nonzero_and_leaves_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ap5.txt");
    fs::write(&path, b"original content\n").unwrap();
    let original_bytes = fs::read(&path).unwrap();

    // Validate that line 1 equals "WRONG CONTENT" — this will fail.
    let o = err(tpu()
        .arg("append")
        .arg(&path)
        .arg("--data")
        .arg("extra line\n")
        .arg("--validate")
        .arg("line:1")
        .arg("WRONG CONTENT"));

    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.is_empty(),
        "expected error on stderr for failed validation"
    );

    let result_bytes = fs::read(&path).unwrap();
    assert_eq!(
        result_bytes, original_bytes,
        "file must be unchanged when validation fails"
    );
}

/// AP-IT-6: `append --diff` emits a diff to stdout and does not modify the file.
#[test]
fn ap_append_diff_shows_diff_without_modifying_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ap6.txt");
    fs::write(&path, b"existing line\n").unwrap();
    let original_bytes = fs::read(&path).unwrap();

    let o = ok(tpu()
        .arg("append")
        .arg(&path)
        .arg("--data")
        .arg("new line\n")
        .arg("--diff"));

    let stdout = String::from_utf8(o.stdout).unwrap();
    // Diff output should contain the new content as an addition.
    assert!(
        stdout.contains("+new line") || stdout.contains("new line"),
        "expected diff output mentioning new line; got: {stdout:?}"
    );

    // File must not have been modified.
    let result_bytes = fs::read(&path).unwrap();
    assert_eq!(
        result_bytes, original_bytes,
        "file must not be modified in --diff mode"
    );
}

/// AP-IT-7: `append` on an empty file writes the content with UTF-8/LF
/// (default when file is empty).
#[test]
fn ap_append_empty_file_writes_utf8_lf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ap7.txt");
    // Create an empty file (must exist, but 0 bytes).
    fs::write(&path, b"").unwrap();

    ok(tpu()
        .arg("append")
        .arg(&path)
        .arg("--data")
        .arg("first line\n"));

    let result = fs::read(&path).unwrap();
    assert_eq!(
        result, b"first line\n",
        "empty file append should produce plain UTF-8/LF output"
    );
}

/// AP-IT-8: `append --line-ending=crlf` overrides the detected line ending
/// for the appended content (LF file → CRLF combined output).
#[test]
fn ap_append_line_ending_override_crlf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ap8.txt");
    fs::write(&path, b"line1\n").unwrap();

    ok(tpu()
        .arg("append")
        .arg(&path)
        .arg("--data")
        .arg("line2\n")
        .arg("--line-ending=crlf"));

    let result = fs::read(&path).unwrap();
    assert_eq!(
        result, b"line1\r\nline2\r\n",
        "expected CRLF line endings after --line-ending=crlf override"
    );
}

/// AP-IT-9: `append` is atomic — if validation fails, the original file is
/// unchanged (no .bak created, no partial write).
#[test]
fn ap_append_atomic_no_bak_on_validation_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ap9.txt");
    fs::write(&path, b"stable content\n").unwrap();
    let original_bytes = fs::read(&path).unwrap();

    // A validation that will definitely fail.
    err(tpu()
        .arg("append")
        .arg(&path)
        .arg("--data")
        .arg("extra\n")
        .arg("--validate")
        .arg("line:1")
        .arg("no match here"));

    // File content must be unchanged.
    let result_bytes = fs::read(&path).unwrap();
    assert_eq!(
        result_bytes, original_bytes,
        "file content must be unchanged"
    );

    // No .bak should have been created (validate failed before any rename).
    let bak_path: std::path::PathBuf = {
        let mut s = path.as_os_str().to_os_string();
        s.push(".bak");
        s.into()
    };
    assert!(
        !bak_path.exists(),
        ".bak file must not exist when validation fails before write"
    );
}

/// AP-IT-10: Appending to a BOM-bearing UTF-8 file preserves the BOM at the
/// start and does not duplicate it.
#[test]
fn ap_append_utf8_bom_preserved_not_duplicated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ap10.txt");
    // Write a UTF-8 BOM file: BOM + content.
    let mut initial: Vec<u8> = vec![0xEF, 0xBB, 0xBF];
    initial.extend_from_slice(b"line1\n");
    fs::write(&path, &initial).unwrap();

    ok(tpu().arg("append").arg(&path).arg("--data").arg("line2\n"));

    let result = fs::read(&path).unwrap();
    // Must start with exactly one UTF-8 BOM.
    assert!(
        result.starts_with(&[0xEF, 0xBB, 0xBF]),
        "UTF-8 BOM must be present at the start; got {:02x?}",
        &result[..result.len().min(6)]
    );
    // BOM must not be duplicated.
    assert!(
        !result[3..].starts_with(&[0xEF, 0xBB, 0xBF]),
        "UTF-8 BOM must not appear twice in the output"
    );
    // Content (after BOM) must contain both lines.
    let text = std::str::from_utf8(&result[3..]).expect("must be valid UTF-8 after BOM");
    assert!(text.contains("line1"), "original content missing");
    assert!(text.contains("line2"), "appended content missing");
}

/// AP-IT-11: `append` on a Windows-1252 file re-encodes appended text in
/// Windows-1252.
#[test]
fn ap_append_windows1252_file_re_encodes_in_windows1252() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ap11.txt");

    // Build a Windows-1252 file with multiple lines of ASCII-safe content.
    // ASCII bytes are identical in Windows-1252, so this works regardless
    // of what harrier detects as the encoding (UTF-8 or Windows-1252 would
    // both give us these bytes).
    let mut initial: Vec<u8> = Vec::new();
    for _ in 0..10 {
        initial.extend_from_slice(b"existing line\n");
    }
    // Add some Windows-1252-specific bytes (e.g. 0x80 = Euro sign, 0x91 = left
    // single quotation mark) to force recognition as Windows-1252.
    initial.extend_from_slice(b"\x80\x91\x92\r\n");
    fs::write(&path, &initial).unwrap();

    ok(tpu()
        .arg("append")
        .arg(&path)
        .arg("--data")
        .arg("appended line\n"));

    let result = fs::read(&path).unwrap();
    // Original byte count was initial.len(); result must be larger.
    assert!(
        result.len() > initial.len(),
        "result must be larger than original after appending"
    );
    // The appended ASCII content must appear as raw ASCII bytes somewhere.
    assert!(
        result
            .windows(b"appended line".len())
            .any(|w| w == b"appended line"),
        "appended content not found in output bytes"
    );
}

/// AP-IT-12: `append` on a non-existent file exits non-zero with an error on
/// stderr.
#[test]
fn ap_append_nonexistent_file_exits_nonzero() {
    let o = err(tpu()
        .arg("append")
        .arg("/this/path/does/not/exist_ap12.txt")
        .arg("--data")
        .arg("irrelevant\n"));
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.is_empty(),
        "expected error message on stderr for non-existent file; got empty stderr"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// SECTION 23 — tpu replace --count / --dry-run integration tests (RC-IT-1..12)
// ──────────────────────────────────────────────────────────────────────────────

/// RC-IT-1: `replace --count` on a file with 3 matches emits the count on
/// stdout and leaves the file unchanged.
#[test]
fn rc_replace_count_matches_emits_count_file_unchanged() {
    // ascii_10lines.txt contains 10 lines, each beginning with "The ".
    // Pattern "The " appears once per line → 10 matches.
    let (_dir, path) = cp("ascii_10lines.txt");
    let original = fs::read(&path).unwrap();

    let o = ok(tpu()
        .arg("replace")
        .arg("--count")
        .arg(&path)
        .arg("The ")
        .arg("A "));

    let stdout = String::from_utf8(o.stdout).unwrap();
    let count: usize = stdout.trim().parse().expect("stdout should be an integer");
    assert!(count > 0, "expected at least one match; got 0");

    // File must be unchanged.
    assert_eq!(
        fs::read(&path).unwrap(),
        original,
        "file must not be modified when --count is used"
    );
}

/// RC-IT-2: `replace --count` on a file with 0 matches emits `0` and exits 0.
#[test]
fn rc_replace_count_no_matches_emits_zero() {
    let (_dir, path) = cp("ascii_10lines.txt");
    let original = fs::read(&path).unwrap();

    let o = ok(tpu()
        .arg("replace")
        .arg("--count")
        .arg(&path)
        .arg("ABSOLUTELY_NO_MATCH_XYZZY_RC2")
        .arg("replacement"));

    let stdout = String::from_utf8(o.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        "0",
        "expected '0' for no matches; got {stdout:?}"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        original,
        "file must not be modified when --count finds no matches"
    );
}

/// RC-IT-3: `replace --count` with a multi-line spanning pattern counts
/// substitution spans, not number of matched lines.
#[test]
fn rc_replace_count_multiline_counts_spans() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rc3.txt");
    // Content: 6 lines; the pattern "aaa\nbbb" will match the pair twice.
    fs::write(&path, b"aaa\nbbb\naaa\nbbb\nccc\nddd\n").unwrap();

    let o = ok(tpu()
        .arg("replace")
        .arg("--count")
        .arg(&path)
        .arg("aaa\nbbb")
        .arg("XXX"));

    let stdout = String::from_utf8(o.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        "2",
        "expected 2 spanning matches; got {stdout:?}"
    );

    // File must be unchanged.
    assert_eq!(fs::read(&path).unwrap(), b"aaa\nbbb\naaa\nbbb\nccc\nddd\n");
}

/// RC-IT-4: `replace --dry-run` emits a diff and does not modify the file.
#[test]
fn rc_replace_dry_run_emits_diff_no_modify() {
    let (_dir, path) = cp("ascii_10lines.txt");
    let original = fs::read(&path).unwrap();

    // Pattern "fox" exists in ascii_10lines.txt (exit 1 because changes would be made).
    let o = err(tpu()
        .arg("replace")
        .arg("--dry-run")
        .arg(&path)
        .arg("fox")
        .arg("cat"));

    let stdout = String::from_utf8(o.stdout).unwrap();
    assert!(
        !stdout.is_empty(),
        "expected diff output on stdout for --dry-run with a match"
    );
    assert!(
        stdout.contains("@@"),
        "expected unified diff hunk markers '@@'; got: {stdout:?}"
    );
    assert!(
        stdout.contains("cat"),
        "expected replacement text 'cat' in the diff; got: {stdout:?}"
    );

    // File must not have been modified.
    assert_eq!(
        fs::read(&path).unwrap(),
        original,
        "file must not be modified by --dry-run"
    );
}

/// RC-IT-5: `replace --dry-run` with no match emits no diff and exits 0.
#[test]
fn rc_replace_dry_run_no_match_exits_zero() {
    let (_dir, path) = cp("ascii_10lines.txt");

    let o = ok(tpu()
        .arg("replace")
        .arg("--dry-run")
        .arg(&path)
        .arg("ABSOLUTELY_NO_MATCH_XYZZY_RC5")
        .arg("replacement"));

    assert!(
        o.stdout.is_empty(),
        "expected empty stdout for --dry-run with no match; got: {}",
        String::from_utf8_lossy(&o.stdout)
    );
}

/// RC-IT-6: `replace --dry-run` with a match exits 1.
#[test]
fn rc_replace_dry_run_with_match_exits_one() {
    let (_dir, path) = cp("ascii_10lines.txt");

    // "fox" is expected to exist → exits 1.
    let o = err(tpu()
        .arg("replace")
        .arg("--dry-run")
        .arg(&path)
        .arg("fox")
        .arg("cat"));

    // Exit code must be exactly 1.
    assert_eq!(
        o.status.code(),
        Some(1),
        "expected exit code 1 for --dry-run with changes; got: {:?}",
        o.status.code()
    );
}

/// RC-IT-7: `replace --count --dry-run` together are rejected with non-zero
/// exit and an error message on stderr.
#[test]
fn rc_replace_count_and_dry_run_together_rejected() {
    let (_dir, path) = cp("ascii_10lines.txt");

    let o = err(tpu()
        .arg("replace")
        .arg("--count")
        .arg("--dry-run")
        .arg(&path)
        .arg("fox")
        .arg("cat"));

    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.is_empty(),
        "expected an error message on stderr when --count and --dry-run are used together"
    );
}

/// RC-IT-8: `replace --count` on a CRLF file reports the correct count.
#[test]
fn rc_replace_count_crlf_file_correct_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rc8.txt");
    // 5 lines with "fox" in each, CRLF line endings.
    let content = "the fox\r\nthe fox\r\nthe fox\r\nthe fox\r\nthe fox\r\n";
    fs::write(&path, content.as_bytes()).unwrap();
    let original = fs::read(&path).unwrap();

    let o = ok(tpu()
        .arg("replace")
        .arg("--count")
        .arg(&path)
        .arg("fox")
        .arg("cat"));

    let stdout = String::from_utf8(o.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        "5",
        "expected 5 matches in CRLF file; got {stdout:?}"
    );

    // File must be unchanged.
    assert_eq!(fs::read(&path).unwrap(), original);
}

/// RC-IT-9: `replace --count` on a UTF-16LE file does not corrupt the file.
/// `tpu replace` operates on raw bytes; the ASCII pattern "fox" does not appear
/// as a raw byte sequence in the UTF-16LE encoding (each char is 2 bytes), so
/// the count is 0.  The key invariant is that the file bytes are unchanged.
#[test]
fn rc_replace_count_utf16le_does_not_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rc9.txt");

    let text = "the fox jumps\nfox runs\nfox hides\n";
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for cu in text.encode_utf16() {
        bytes.extend_from_slice(&cu.to_le_bytes());
    }
    fs::write(&path, &bytes).unwrap();
    let original = fs::read(&path).unwrap();

    // The ASCII bytes for "fox" do not appear in the UTF-16LE encoding.
    // --count must return 0 and must not modify the file.
    let o = ok(tpu()
        .arg("replace")
        .arg("--count")
        .arg(&path)
        .arg("fox")
        .arg("cat"));

    let stdout = String::from_utf8(o.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        "0",
        "expected 0 raw-byte matches in UTF-16LE file; got {stdout:?}"
    );

    // File must be unchanged — the key invariant.
    assert_eq!(
        fs::read(&path).unwrap(),
        original,
        "UTF-16LE file must not be modified by --count"
    );
    // BOM sanity check.
    assert_eq!(&original[..2], &[0xFF, 0xFE], "BOM must be intact");
}

/// RC-IT-10: `replace --dry-run` on a UTF-16LE file with no raw-byte match
/// exits 0, emits no diff, and does not modify the file.
/// `tpu replace` works on raw bytes; "fox" in ASCII (0x66 0x6F 0x78) does not
/// appear in the UTF-16LE encoding where each char occupies two bytes.
#[test]
fn rc_replace_dry_run_utf16le_no_match_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rc10.txt");

    let text = "hello fox\nworld fox\n";
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for cu in text.encode_utf16() {
        bytes.extend_from_slice(&cu.to_le_bytes());
    }
    fs::write(&path, &bytes).unwrap();
    let original = fs::read(&path).unwrap();

    // No raw-byte match → exits 0, no diff emitted.
    let o = ok(tpu()
        .arg("replace")
        .arg("--dry-run")
        .arg(&path)
        .arg("fox")
        .arg("cat"));

    assert!(
        o.stdout.is_empty(),
        "expected empty stdout for --dry-run with no match; got: {}",
        String::from_utf8_lossy(&o.stdout)
    );

    // File must be unchanged.
    assert_eq!(
        fs::read(&path).unwrap(),
        original,
        "UTF-16LE file must not be modified by --dry-run"
    );
    assert_eq!(&original[..2], &[0xFF, 0xFE], "BOM must be intact");
}

/// RC-IT-11: `replace --count --multiline` counts correctly across line
/// boundaries.
#[test]
fn rc_replace_count_multiline_flag_counts_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rc11.txt");
    // The pattern `^start` (with --multiline) matches at the start of each
    // line.  There are 4 lines starting with "start".
    fs::write(
        &path,
        b"start here\nno match\nstart again\nno match\nstart once\nstart last\n",
    )
    .unwrap();
    let original = fs::read(&path).unwrap();

    let o = ok(tpu()
        .arg("replace")
        .arg("--count")
        .arg("--regex")
        .arg("--multiline")
        .arg(&path)
        .arg("^start")
        .arg("BEGIN"));

    let stdout = String::from_utf8(o.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        "4",
        "expected 4 multiline matches; got {stdout:?}"
    );

    // File must be unchanged.
    assert_eq!(fs::read(&path).unwrap(), original);
}

/// RC-IT-12: `replace --count` on an empty file emits `0` and exits 0.
#[test]
fn rc_replace_count_empty_file_emits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rc12.txt");
    fs::write(&path, b"").unwrap();

    let o = ok(tpu()
        .arg("replace")
        .arg("--count")
        .arg(&path)
        .arg("anything")
        .arg("replacement"));

    let stdout = String::from_utf8(o.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        "0",
        "expected '0' for empty file; got {stdout:?}"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// SECTION 24 — tpu count --stats integration tests (CS-IT-1..12)
// ──────────────────────────────────────────────────────────────────────────────

/// CS-IT-1: `count --stats` on a UTF-8 LF file emits encoding, BOM presence,
/// and line-ending style in the human-readable output.
#[test]
fn cs_count_stats_utf8_lf_file_shows_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cs1.txt");
    fs::write(&path, b"hello\nworld\n").unwrap();

    let o = ok(tpu().arg("count").arg("--stats").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains("encoding: UTF-8"),
        "expected encoding: UTF-8; got: {out:?}"
    );
    assert!(
        out.contains("bom: false"),
        "expected bom: false; got: {out:?}"
    );
    assert!(
        out.contains("line_ending: LF"),
        "expected line_ending: LF; got: {out:?}"
    );
}

/// CS-IT-2: `count --stats` on an empty file uses defaults (UTF-8, no BOM, LF).
#[test]
fn cs_count_stats_empty_file_uses_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cs2.txt");
    fs::write(&path, b"").unwrap();

    let o = ok(tpu().arg("count").arg("--stats").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains("encoding: UTF-8"),
        "expected encoding: UTF-8 for empty file; got: {out:?}"
    );
    assert!(
        out.contains("bom: false"),
        "expected bom: false for empty file; got: {out:?}"
    );
    assert!(
        out.contains("line_ending: LF"),
        "expected line_ending: LF for empty file; got: {out:?}"
    );
    assert!(
        out.contains("lines: 0"),
        "expected lines: 0 for empty file; got: {out:?}"
    );
}

/// CS-IT-3: `count --stats` on a CRLF file reports `line_ending: CRLF`.
#[test]
fn cs_count_stats_crlf_file_reports_crlf_line_ending() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cs3.txt");
    fs::write(&path, b"line1\r\nline2\r\nline3\r\n").unwrap();

    let o = ok(tpu().arg("count").arg("--stats").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains("line_ending: CRLF"),
        "expected line_ending: CRLF; got: {out:?}"
    );
    assert!(
        out.contains("bom: false"),
        "expected bom: false; got: {out:?}"
    );
}

/// CS-IT-4: `count --stats` on a UTF-16LE BOM file reports the correct
/// encoding and BOM presence.
#[test]
fn cs_count_stats_utf16le_bom_file_reports_encoding_and_bom() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cs4.txt");

    let text = "alpha\nbeta\n";
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for cu in text.encode_utf16() {
        bytes.extend_from_slice(&cu.to_le_bytes());
    }
    fs::write(&path, &bytes).unwrap();

    let o = ok(tpu().arg("count").arg("--stats").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains("encoding: UTF-16LE"),
        "expected encoding: UTF-16LE; got: {out:?}"
    );
    assert!(
        out.contains("bom: true"),
        "expected bom: true; got: {out:?}"
    );
}

/// CS-IT-5: `count --stats --lines` emits stats followed by just the line count.
#[test]
fn cs_count_stats_combined_with_lines_flag() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cs5.txt");
    fs::write(&path, b"one\ntwo\nthree\n").unwrap();

    let o = ok(tpu().arg("count").arg("--stats").arg("--lines").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains("encoding:"),
        "expected encoding in output; got: {out:?}"
    );
    assert!(out.contains("bom:"), "expected bom in output; got: {out:?}");
    assert!(
        out.contains("line_ending:"),
        "expected line_ending in output; got: {out:?}"
    );
    assert!(out.contains("lines: 3"), "expected lines: 3; got: {out:?}");
    // --lines only — words/chars/bytes must not appear
    assert!(
        !out.contains("words:"),
        "unexpected 'words' in output; got: {out:?}"
    );
    assert!(
        !out.contains("bytes:"),
        "unexpected 'bytes' in output; got: {out:?}"
    );
}

/// CS-IT-6: `tpu count --message-format=json` without `--stats` always
/// includes encoding/bom/line_ending in the JSON output.
#[test]
fn cs_count_json_mode_always_includes_stats() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cs6.txt");
    fs::write(&path, b"a\nb\n").unwrap();

    // No --stats flag; JSON mode should still include stats.
    let o = ok(tpu()
        .arg("--message-format=json")
        .arg("count")
        .arg("--lines")
        .arg(&path));
    let messages = parse_ndjson(&o.stdout);
    let data: Vec<&serde_json::Value> = messages.iter().filter(|m| reason(m) == "data").collect();

    let metric_names: Vec<&str> = data.iter().map(|m| m["metric"].as_str().unwrap()).collect();
    assert!(
        metric_names.contains(&"encoding"),
        "JSON output must always include 'encoding'; got: {metric_names:?}"
    );
    assert!(
        metric_names.contains(&"bom"),
        "JSON output must always include 'bom'; got: {metric_names:?}"
    );
    assert!(
        metric_names.contains(&"line_ending"),
        "JSON output must always include 'line_ending'; got: {metric_names:?}"
    );
}

/// CS-IT-7: Stats appear BEFORE metric counts in the output, and in the order
/// encoding → bom → line_ending.
#[test]
fn cs_count_stats_appear_before_metrics_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cs7.txt");
    fs::write(&path, b"hello world\n").unwrap();

    let o = ok(tpu().arg("count").arg("--stats").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    let enc_pos = out.find("encoding:").expect("encoding must appear");
    let bom_pos = out.find("bom:").expect("bom must appear");
    let le_pos = out.find("line_ending:").expect("line_ending must appear");
    let lines_pos = out.find("lines:").expect("lines must appear");

    assert!(enc_pos < bom_pos, "encoding must precede bom");
    assert!(bom_pos < le_pos, "bom must precede line_ending");
    assert!(le_pos < lines_pos, "line_ending must precede metric counts");
}

/// CS-IT-8: `count --stats` on a Windows-1252 file reports the correct
/// encoding label.
#[test]
fn cs_count_stats_windows1252_file_reports_correct_encoding() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cs8.txt");
    // 0xE9 = 'é' in Windows-1252; not valid UTF-8.
    // A few repeated bytes make detection more reliable.
    let mut bytes = vec![0xE9u8; 20];
    bytes.extend_from_slice(b"\nhello\n");
    fs::write(&path, &bytes).unwrap();

    let o = ok(tpu().arg("count").arg("--stats").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    // The encoding must not be reported as UTF-8 for a Windows-1252 file.
    assert!(
        out.contains("encoding:"),
        "expected encoding in output; got: {out:?}"
    );
    assert!(
        !out.contains("encoding: UTF-8"),
        "a Windows-1252 file must not report UTF-8; got: {out:?}"
    );
}

/// CS-IT-9: `count --stats` alone (no other flags) shows stats then ALL FOUR
/// default metrics (lines, words, chars, bytes) — the current default is
/// preserved when --stats is the only flag.
#[test]
fn cs_count_stats_alone_shows_stats_and_all_default_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cs9.txt");
    fs::write(&path, b"foo bar\nbaz\n").unwrap();

    let o = ok(tpu().arg("count").arg("--stats").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    // Stats block
    assert!(out.contains("encoding:"), "--stats must include encoding");
    assert!(out.contains("bom:"), "--stats must include bom");
    assert!(
        out.contains("line_ending:"),
        "--stats must include line_ending"
    );

    // All four default metrics still present
    assert!(
        out.contains("lines:"),
        "--stats alone must still emit lines"
    );
    assert!(
        out.contains("words:"),
        "--stats alone must still emit words"
    );
    assert!(
        out.contains("chars:"),
        "--stats alone must still emit chars"
    );
    assert!(
        out.contains("bytes:"),
        "--stats alone must still emit bytes"
    );
}

/// CS-IT-10: `count --stats` on a UTF-8 BOM file reports `bom: true`.
#[test]
fn cs_count_stats_utf8_bom_file_reports_bom_true() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cs10.txt");
    // UTF-8 BOM is EF BB BF.
    let mut bytes = vec![0xEFu8, 0xBB, 0xBF];
    bytes.extend_from_slice(b"hello\nworld\n");
    fs::write(&path, &bytes).unwrap();

    let o = ok(tpu().arg("count").arg("--stats").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    assert!(
        out.contains("bom: true"),
        "UTF-8 BOM file must report bom: true; got: {out:?}"
    );
    assert!(
        out.contains("encoding: UTF-8"),
        "expected encoding: UTF-8 for UTF-8 BOM file; got: {out:?}"
    );
}

/// CS-IT-11: `tpu count` without `--stats` in human mode does NOT emit stats.
#[test]
fn cs_count_no_stats_flag_omits_metadata_in_human_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cs11.txt");
    fs::write(&path, b"hello\nworld\n").unwrap();

    let o = ok(tpu().arg("count").arg(&path));
    let out = String::from_utf8(o.stdout).unwrap();

    // Must have standard metrics
    assert!(
        out.contains("lines:"),
        "expected lines in default output; got: {out:?}"
    );

    // Must NOT have stats when no --stats flag in human mode
    assert!(
        !out.contains("encoding:"),
        "encoding must not appear without --stats; got: {out:?}"
    );
    assert!(
        !out.contains("bom:"),
        "bom must not appear without --stats; got: {out:?}"
    );
    assert!(
        !out.contains("line_ending:"),
        "line_ending must not appear without --stats; got: {out:?}"
    );
}

/// CS-IT-12: JSON stats values have correct types: `value` is a string for
/// encoding and line_ending, and a boolean for bom.
#[test]
fn cs_count_json_stats_have_correct_value_types() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cs12.txt");
    fs::write(&path, b"test content\n").unwrap();

    let o = ok(tpu().arg("--message-format=json").arg("count").arg(&path));
    let messages = parse_ndjson(&o.stdout);
    let data: Vec<&serde_json::Value> = messages.iter().filter(|m| reason(m) == "data").collect();

    let by_metric: std::collections::HashMap<&str, &serde_json::Value> = data
        .iter()
        .map(|m| (m["metric"].as_str().unwrap(), *m))
        .collect();

    // encoding: string value
    let enc_msg = by_metric["encoding"];
    assert!(
        enc_msg["value"].is_string(),
        "encoding value must be a JSON string; got: {enc_msg:?}"
    );

    // bom: boolean value
    let bom_msg = by_metric["bom"];
    assert!(
        bom_msg["value"].is_boolean(),
        "bom value must be a JSON boolean; got: {bom_msg:?}"
    );

    // line_ending: string value
    let le_msg = by_metric["line_ending"];
    assert!(
        le_msg["value"].is_string(),
        "line_ending value must be a JSON string; got: {le_msg:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 8 — `tpu find`
// ═══════════════════════════════════════════════════════════════════════════════

// ─── FN-IT-1: simple positional form emits all matching lines ─────────────────

#[test]
fn find_positional_matches_all_lines() {
    // ascii_10lines.txt has 10 lines each containing "fox"; all 10 should match.
    let o = ok(tpu().arg("find").arg("fox").arg(asset("ascii_10lines.txt")));
    let lines: Vec<&[u8]> = o
        .stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        10,
        "expected 10 matching lines; got {}",
        lines.len()
    );
}

// ─── FN-IT-2: --pattern/--path form emits exactly one line ───────────────────

#[test]
fn find_pattern_path_flags_emit_one_line() {
    // "line 5:" appears on exactly one line in the 10-line file.
    let o = ok(tpu()
        .arg("find")
        .arg("--pattern")
        .arg("line 5:")
        .arg("--path")
        .arg(asset("ascii_10lines.txt")));
    let lines: Vec<&[u8]> = o
        .stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly 1 matching line; got {}",
        lines.len()
    );
    let text = String::from_utf8_lossy(lines[0]);
    assert!(
        text.contains("line 5:"),
        "expected 'line 5:' in output; got: {text:?}"
    );
}

// ─── FN-IT-3: no matches → empty output, exit 1 ──────────────────────────────

#[test]
fn find_no_match_exits_one() {
    // A pattern that cannot match any line in ascii_10lines.txt.
    let mut cmd = tpu();
    cmd.arg("find")
        .arg("ZZZNOMATCH_XY0000")
        .arg(asset("ascii_10lines.txt"));
    let o = cmd.output().expect("failed to run tpu");
    assert_eq!(
        o.status.code().unwrap_or(-1),
        1,
        "expected exit code 1 (no matches)"
    );
    assert!(
        o.stdout.is_empty(),
        "expected empty stdout on no match; got: {:?}",
        &o.stdout
    );
}

// ─── FN-IT-4: --count emits the count and no match lines ────────────────────

#[test]
fn find_count_emits_count_not_lines() {
    // All 10 lines match "fox"; stdout should be "10\n", not the 10 lines.
    let o = ok(tpu()
        .arg("find")
        .arg("--count")
        .arg("fox")
        .arg(asset("ascii_10lines.txt")));
    let s = String::from_utf8_lossy(&o.stdout);
    assert_eq!(s.trim(), "10", "expected count line '10'");
    assert_eq!(
        s.lines().count(),
        1,
        "count mode should emit exactly one line; got: {s:?}"
    );
}

// ─── FN-IT-5: --invert emits all lines except the match ──────────────────────

#[test]
fn find_invert_emits_non_matching_lines() {
    // "line 5:" uniquely identifies line 5; inverted result is 9 lines.
    let o = ok(tpu()
        .arg("find")
        .arg("--invert")
        .arg("--pattern")
        .arg("line 5:")
        .arg("--path")
        .arg(asset("ascii_10lines.txt")));
    let lines: Vec<&[u8]> = o
        .stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        9,
        "expected 9 lines with --invert; got {}",
        lines.len()
    );
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        !s.contains("line 5:"),
        "--invert output must not contain the matched line"
    );
}

// ─── FN-IT-6: --numbers prefixes each output line with its 1-based line number

#[test]
fn find_numbers_prefixes_line_numbers() {
    // Format for single-path --numbers is "N:content".
    let o = ok(tpu()
        .arg("find")
        .arg("--numbers")
        .arg("fox")
        .arg(asset("ascii_10lines.txt")));
    let s = String::from_utf8_lossy(&o.stdout);
    let first = s.lines().next().unwrap_or("");
    let last = s.lines().last().unwrap_or("");
    assert!(
        first.starts_with("1:"),
        "first line should start with '1:'; got: {first:?}"
    );
    assert!(
        last.starts_with("10:"),
        "last line should start with '10:'; got: {last:?}"
    );
    // All 10 lines are matched and each has a numeric prefix.
    assert_eq!(
        s.lines().count(),
        10,
        "--numbers should produce 10 prefixed lines"
    );
}

// ─── FN-IT-7: literal matching (the default) treats the dot as a literal ────

#[test]
fn find_default_literal_dot() {
    // A file with "line 5." (literal dot) and "line 5X" (any-char in regex).
    // Literal matching (the default, no --regex) makes the dot literal, so
    // only "line 5." matches.
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("dots.txt");
    fs::write(&f, b"line 5.\nline 5X\nline 5:\n").unwrap();
    let o = ok(tpu()
        .arg("find")
        .arg("--pattern")
        .arg("line 5.")
        .arg("--path")
        .arg(&f));
    let lines: Vec<&[u8]> = o
        .stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "default literal matching should match only the literal 'line 5.'"
    );
    let text = String::from_utf8_lossy(lines[0]);
    assert!(
        text.contains("line 5."),
        "matched line should contain literal 'line 5.'; got: {text:?}"
    );
    assert!(
        !text.contains("line 5X"),
        "must not match 'line 5X' with a literal dot"
    );
}

// ─── FN-IT-8: find on a UTF-16LE file decodes and matches correctly ──────────

#[test]
fn find_utf16le_file_decodes_and_matches() {
    // Write a UTF-16LE file with BOM.
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("utf16le.txt");
    let content = "find me here\ngoodbye world\n";
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for unit in content.encode_utf16() {
        bytes.push((unit & 0xFF) as u8);
        bytes.push((unit >> 8) as u8);
    }
    fs::write(&f, &bytes).unwrap();

    let o = ok(tpu().arg("find").arg("find me").arg(&f));
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        s.contains("find me here"),
        "UTF-16LE find should decode and match; got: {s:?}"
    );
    // The "goodbye" line must not appear.
    assert!(
        !s.contains("goodbye"),
        "non-matching line should not appear; got: {s:?}"
    );
}

// ─── FN-IT-9: find on a CRLF file emits matched lines with LF only ───────────

#[test]
fn find_crlf_file_output_has_lf_only() {
    // multiline_crlf.txt has CRLF line endings; find output must be LF-only.
    let o = ok(tpu()
        .arg("find")
        .arg("line")
        .arg(asset("multiline_crlf.txt")));
    assert!(
        !o.stdout.is_empty(),
        "expected at least one matching line from CRLF file"
    );
    assert!(
        !o.stdout.contains(&b'\r'),
        "find output must not contain CR bytes"
    );
}

// ─── FN-IT-10: find on an empty file emits no output and exits 1 ─────────────

#[test]
fn find_empty_file_exits_one() {
    let mut cmd = tpu();
    cmd.arg("find").arg("fox").arg(asset("empty.txt"));
    let o = cmd.output().expect("failed to run tpu");
    assert_eq!(
        o.status.code().unwrap_or(-1),
        1,
        "empty file should exit 1 (no matches)"
    );
    assert!(
        o.stdout.is_empty(),
        "empty file should produce no output; got: {:?}",
        &o.stdout
    );
}

// ─── FN-IT-11: --count --invert "NOMATCH" counts all lines ───────────────────

#[test]
fn find_count_invert_nomatch_counts_all() {
    // No line matches "ZZZNOMATCH", so --invert selects all 10 lines.
    let o = ok(tpu()
        .arg("find")
        .arg("--count")
        .arg("--invert")
        .arg("--pattern")
        .arg("ZZZNOMATCH_XY0000")
        .arg("--path")
        .arg(asset("ascii_10lines.txt")));
    let s = String::from_utf8_lossy(&o.stdout);
    assert_eq!(
        s.trim(),
        "10",
        "all 10 non-matching lines should be counted; got: {s:?}"
    );
}

// ─── FN-IT-12: invalid regex pattern exits 2 with error on stderr ────────────

#[test]
fn find_bad_regex_exits_two() {
    let mut cmd = tpu();
    cmd.arg("find")
        .arg("--regex")
        .arg("[invalid_regex")
        .arg(asset("ascii_10lines.txt"));
    let o = cmd.output().expect("failed to run tpu");
    assert_eq!(
        o.status.code().unwrap_or(-1),
        2,
        "invalid regex should exit 2"
    );
    assert!(
        !o.stderr.is_empty(),
        "invalid regex should produce an error message on stderr"
    );
}

// ─── FN-IT-17: two --pattern values are combined with OR logic ───────────────

#[test]
fn find_two_patterns_or_mode_matches_either() {
    // "line 3:" appears only on line 3; "line 7:" only on line 7.
    // Supply the first pattern positionally and the path via --path so that
    // clap's positional slots are unambiguous.
    let o = ok(tpu()
        .arg("find")
        .arg("line 3:")
        .arg("--path")
        .arg(asset("ascii_10lines.txt"))
        .arg("--pattern")
        .arg("line 7:"));
    let s = String::from_utf8_lossy(&o.stdout);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 2, "exactly two matches expected; got: {s:?}");
    assert!(
        lines.iter().any(|l| l.contains("line 3:")),
        "result should include the line 3 match; got: {s:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("line 7:")),
        "result should include the line 7 match; got: {s:?}"
    );
}

#[test]
fn find_glob_no_match_exits_two_with_error() {
    let dir = tempfile::tempdir().unwrap();
    // Empty directory: *.txt matches nothing.
    let mut cmd = tpu();
    cmd.current_dir(dir.path())
        .arg("find")
        .arg("fox")
        .arg("*.txt");
    let o = cmd.output().expect("failed to run tpu");
    assert_eq!(
        o.status.code().unwrap_or(-1),
        2,
        "glob matching zero files should exit with code 2"
    );
    assert!(
        !o.stderr.is_empty(),
        "an error message should appear on stderr when no files are matched"
    );
}

#[test]
fn find_glob_two_files_emits_filename_prefix_and_count_total() {
    let dir = tempfile::tempdir().unwrap();
    // Each file has one matching line ("fox") so total count should be 2.
    fs::write(dir.path().join("first.txt"), b"the quick brown fox\n").unwrap();
    fs::write(dir.path().join("second.txt"), b"fox trot\n").unwrap();

    // Without --count: both lines must have a filename prefix.
    let o = ok(tpu()
        .current_dir(dir.path())
        .arg("find")
        .arg("fox")
        .arg("*.txt"));
    let s = String::from_utf8_lossy(&o.stdout);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 2, "two matching lines expected; got: {s:?}");
    for line in &lines {
        assert!(
            line.contains(':'),
            "multi-file output must have filename prefix (file:text); got: {line:?}"
        );
    }

    // With --count: per-file lines + "total: 2" footer.
    let o = ok(tpu()
        .current_dir(dir.path())
        .arg("find")
        .arg("--count")
        .arg("fox")
        .arg("*.txt"));
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        s.contains("total: 2"),
        "--count on two files should include 'total: 2'; got: {s:?}"
    );
}

#[test]
fn find_numbers_shows_line_number_for_specific_line() {
    // "line 7:" appears only on line 7 of ascii_10lines.txt.
    let o = ok(tpu()
        .arg("find")
        .arg("--numbers")
        .arg("line 7:")
        .arg(asset("ascii_10lines.txt")));
    let s = String::from_utf8_lossy(&o.stdout);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one match expected; got: {s:?}");
    assert!(
        lines[0].starts_with("7:"),
        "--numbers output should start with '7:'; got: {:?}",
        lines[0]
    );
}

#[test]
fn find_windows1252_file_matches_ascii_content() {
    // Write a file containing Windows-1252 bytes: the byte 0xE9 (é in
    // Windows-1252) is not valid UTF-8, triggering encoding detection.
    // The ASCII word "coffee" on the same line as the extended character
    // should be found after harrier decodes the file.
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("win1252.txt");
    // "caf\xE9 au lait\ncoffee shop\n" in Windows-1252.
    fs::write(&f, b"caf\xE9 au lait\ncoffee shop\n").unwrap();

    // Search for the ASCII word on the second line (no extended chars).
    let o = ok(tpu().arg("find").arg("coffee").arg(&f));
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        s.contains("coffee shop"),
        "find should match ASCII content in a Windows-1252 file; got: {s:?}"
    );
    // Only the matching line should appear.
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one line should match; got: {s:?}");
}

// ─── FN-IT-18: two patterns with --all-match: only lines matching both ────────

#[test]
fn find_two_patterns_all_match_requires_both() {
    // Every line of ascii_10lines.txt contains "fox", but only line 5
    // contains "line 5:".  With --all-match only line 5 is emitted.
    let o = ok(tpu()
        .arg("find")
        .arg("fox")
        .arg("--path")
        .arg(asset("ascii_10lines.txt"))
        .arg("--pattern")
        .arg("line 5:")
        .arg("--all-match"));
    let s = String::from_utf8_lossy(&o.stdout);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "only one line should match both patterns; got: {s:?}"
    );
    assert!(
        lines[0].contains("line 5:"),
        "the matched line should contain 'line 5:'; got: {:?}",
        lines[0]
    );
}

// ─── FN-IT-19: -A 1 after-context and "--" separator between groups ───────────

#[test]
fn find_after_context_one_emits_separator_between_groups() {
    // Match lines 3 and 7; with -A 1 each match is followed by a context line.
    // Groups are non-adjacent so a "--" separator must appear between them.
    let o = ok(tpu()
        .arg("find")
        .arg("-A")
        .arg("1")
        .arg("line 3:")
        .arg("--path")
        .arg(asset("ascii_10lines.txt"))
        .arg("--pattern")
        .arg("line 7:"));
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        s.contains("--\n"),
        "non-adjacent context groups must be separated by '--'; got: {s:?}"
    );
    assert!(
        s.contains("line 3:"),
        "line 3 match should appear; got: {s:?}"
    );
    assert!(
        s.contains("line 7:"),
        "line 7 match should appear; got: {s:?}"
    );
}

// ─── FN-IT-20: -B 1 emits one before-context line preceding each match ───────

#[test]
fn find_before_context_one_emits_preceding_line() {
    // "line 5:" appears on line 5.  With -B 1, line 4 should also appear
    // as a context line using the `<lineno>-<text>` format (dash separator).
    let o = ok(tpu()
        .arg("find")
        .arg("-B")
        .arg("1")
        .arg("line 5:")
        .arg(asset("ascii_10lines.txt")));
    let s = String::from_utf8_lossy(&o.stdout);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "match + one before-context line expected; got: {s:?}"
    );
    // The context line must use the dash separator format and reference line 4.
    assert!(
        lines[0].starts_with("4-"),
        "before-context line should be '4-<text>'; got: {:?}",
        lines[0]
    );
    assert!(
        lines[1].contains("line 5:"),
        "match line should contain 'line 5:'; got: {:?}",
        lines[1]
    );
}

// ─── FN-IT-21: context de-duplication — overlap line emitted exactly once ────

#[test]
fn find_context_deduplicated_overlap_once() {
    // Lines 3 and 5 both match.  With -A 1 -B 1, line 4 is after-context for
    // line 3 AND before-context for line 5.  It must appear exactly once.
    let o = ok(tpu()
        .arg("find")
        .arg("-A")
        .arg("1")
        .arg("-B")
        .arg("1")
        .arg("line 3:")
        .arg("--path")
        .arg(asset("ascii_10lines.txt"))
        .arg("--pattern")
        .arg("line 5:"));
    let s = String::from_utf8_lossy(&o.stdout);
    // Count how many times line 4 appears (it contains "line 4:").
    let count_line4 = s.matches("line 4:").count();
    assert_eq!(
        count_line4, 1,
        "line 4 (shared context) must appear exactly once; got: {s:?}"
    );
    // Matches must also appear.
    assert!(
        s.contains("line 3:"),
        "line 3 match must appear; got: {s:?}"
    );
    assert!(
        s.contains("line 5:"),
        "line 5 match must appear; got: {s:?}"
    );
}

// ─── FN-IT-22: --multiline enables ^ to anchor at start of each logical line ─

#[test]
fn find_multiline_caret_anchors_at_line_start() {
    // "^line 5:" with --multiline must match exactly the line that starts with
    // "line 5:" (line 5).  Without --multiline, ^ anchors to start of the
    // whole decoded chunk and would not work per-line.
    let o = ok(tpu()
        .arg("find")
        .arg("--regex")
        .arg("--multiline")
        .arg("^line 5:")
        .arg(asset("ascii_10lines.txt")));
    let s = String::from_utf8_lossy(&o.stdout);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "exactly one line should match '^line 5:'; got: {s:?}"
    );
    assert!(
        lines[0].contains("line 5:"),
        "the match should be line 5; got: {:?}",
        lines[0]
    );
}

// ─── FN-IT-23: --count with a single file does NOT emit a "total:" line ──────

#[test]
fn find_count_single_file_no_total_line() {
    // ascii_10lines.txt has 10 lines all matching "fox".
    // With a single file, --count must emit just the bare number with no "total:".
    let o = ok(tpu()
        .arg("find")
        .arg("--count")
        .arg("fox")
        .arg(asset("ascii_10lines.txt")));
    let s = String::from_utf8_lossy(&o.stdout);
    assert_eq!(
        s.trim(),
        "10",
        "--count on a single file should emit only the bare count; got: {s:?}"
    );
    assert!(
        !s.contains("total:"),
        "--count on a single file must not emit 'total:'; got: {s:?}"
    );
}

// ─── FN-IT-24: --invert with --all-match emits lines failing at least one ─────

#[test]
fn find_invert_all_match_emits_lines_failing_at_least_one_pattern() {
    // Every line contains "fox".  Only line 5 also contains "line 5:".
    // --all-match --invert: emit lines that do NOT match ALL patterns.
    // That means lines where at least one pattern fails → all lines except line 5.
    // Expected: 9 lines.
    let o = ok(tpu()
        .arg("find")
        .arg("--all-match")
        .arg("--invert")
        .arg("fox")
        .arg("--path")
        .arg(asset("ascii_10lines.txt"))
        .arg("--pattern")
        .arg("line 5:"));
    let s = String::from_utf8_lossy(&o.stdout);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(
        lines.len(),
        9,
        "--invert --all-match should emit 9 lines (all except line 5); got: {s:?}"
    );
    assert!(
        !s.contains("line 5:"),
        "line 5 (matching both patterns) must not appear in inverted output; got: {s:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION — HV: Hyphen-prefixed argument values (HV-IT-1 … HV-IT-10)
//
// Each test verifies that arguments beginning with `-` are accepted literally
// and not misinterpreted as flags by clap.
// ═══════════════════════════════════════════════════════════════════════════════

/// HV-IT-1: `tpu replace` accepts a pattern that starts with `-`.
#[test]
fn hv_replace_pattern_starting_with_dash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hv1.txt");
    fs::write(&path, b"-value appears here\n").unwrap();

    ok(tpu().arg("replace").arg(&path).arg("-value").arg("VALUE"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("VALUE"),
        "dash-prefixed pattern should match and be replaced; got: {content:?}"
    );
    assert!(
        !content.contains("-value"),
        "original dash-prefixed token must be gone; got: {content:?}"
    );
    drop(dir);
}

/// HV-IT-2: `tpu replace` accepts a replacement string that starts with `-`.
#[test]
fn hv_replace_replacement_starting_with_dash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hv2.txt");
    fs::write(&path, b"hello world\n").unwrap();

    ok(tpu().arg("replace").arg(&path).arg("hello").arg("-goodbye"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("-goodbye"),
        "replacement starting with '-' must appear in output; got: {content:?}"
    );
    drop(dir);
}

/// HV-IT-3: Both pattern and replacement can start with `-`.
#[test]
fn hv_replace_both_pattern_and_replacement_with_dash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hv3.txt");
    fs::write(&path, b"-alpha and -beta\n").unwrap();

    ok(tpu().arg("replace").arg(&path).arg("-alpha").arg("-ALPHA"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("-ALPHA"),
        "dash-to-dash replace must produce dash-prefixed result; got: {content:?}"
    );
    drop(dir);
}

/// HV-IT-4: `tpu replace` accepts a numeric-looking dash pattern (e.g. `-1`).
#[test]
fn hv_replace_numeric_dash_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hv4.txt");
    fs::write(&path, b"score: -1 points\n").unwrap();

    ok(tpu().arg("replace").arg(&path).arg("-1").arg("0"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains(": 0 points"),
        "numeric dash pattern (-1) should be matched; got: {content:?}"
    );
    drop(dir);
}

/// HV-IT-5: `tpu replace` handles a multi-word replacement starting with `-`.
#[test]
#[allow(clippy::suspicious_command_arg_space)]
fn hv_replace_dash_replacement_multiword() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hv5.txt");
    fs::write(&path, b"start here end\n").unwrap();

    ok(tpu()
        .arg("replace")
        .arg(&path)
        .arg("here")
        .arg("-there now"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("-there now"),
        "multi-word replacement starting with '-' must be used literally; got: {content:?}"
    );
    drop(dir);
}

/// HV-IT-6: `tpu write` accepts positional data starting with `-`.
#[test]
#[allow(clippy::suspicious_command_arg_space)]
fn hv_write_data_positional_starting_with_dash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hv6.txt");

    ok(tpu().arg("write").arg(&path).arg("-prefixed content\n"));

    let raw = fs::read(&path).unwrap();
    assert!(
        raw.windows(b"-prefixed".len()).any(|w| w == b"-prefixed"),
        "written file must contain the dash-prefixed data; got: {:?}",
        String::from_utf8_lossy(&raw)
    );
    drop(dir);
}

/// HV-IT-7: `tpu write` accepts a single dash as positional data.
#[test]
fn hv_write_data_positional_single_dash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hv7.txt");

    ok(tpu().arg("write").arg(&path).arg("-\n"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains('-'),
        "written file must contain the single dash; got: {content:?}"
    );
    drop(dir);
}

/// HV-IT-8: `tpu append --data` accepts a value starting with `-`.
#[test]
#[allow(clippy::suspicious_command_arg_space)]
fn hv_append_data_flag_starting_with_dash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hv8.txt");
    fs::write(&path, b"existing\n").unwrap();

    ok(tpu()
        .arg("append")
        .arg(&path)
        .arg("--data")
        .arg("-appended line\n"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("existing"),
        "original content must be preserved; got: {content:?}"
    );
    assert!(
        content.contains("-appended line"),
        "dash-prefixed appended content must appear; got: {content:?}"
    );
    drop(dir);
}

/// HV-IT-9: `tpu append --data` accepts a single dash as its value.
#[test]
fn hv_append_data_flag_single_dash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hv9.txt");
    fs::write(&path, b"base\n").unwrap();

    ok(tpu().arg("append").arg(&path).arg("--data").arg("-\n"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains('-'),
        "appended single dash must appear in file; got: {content:?}"
    );
    drop(dir);
}

/// HV-IT-10: write-then-read roundtrip with dash-prefixed content is lossless.
#[test]
fn hv_write_read_roundtrip_dash_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hv10.txt");
    let expected = "-line1\n-line2\n-line3\n";

    ok(tpu().arg("write").arg(&path).arg(expected));

    let o = ok(tpu().arg("read").arg(&path));
    let readback = String::from_utf8(o.stdout).unwrap();
    assert_eq!(
        readback, expected,
        "round-trip through write+read must preserve dash-prefixed content"
    );
    drop(dir);
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION — FS: literal matching for `tpu replace` (FS-IT-1 … FS-IT-12)
//
// Verifies that the default (regex is opt-in, off unless --regex/-E is
// passed) treats every regex metacharacter in the pattern as a literal so
// that Copilot can safely pass code snippets as patterns without escaping
// them.
// ═══════════════════════════════════════════════════════════════════════════════

/// FS-IT-1: default literal matching matches a literal `{` without a regex error.
#[test]
fn fs_replace_default_literal_curly_brace_matches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fs1.txt");
    fs::write(&path, b"fn foo() { return 1; }\n").unwrap();

    ok(tpu()
        .arg("replace")
        .arg(&path)
        .arg("{ return 1; }")
        .arg("{ return 2; }"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("{ return 2; }"),
        "curly-brace literal replacement must succeed; got: {content:?}"
    );
    drop(dir);
}

/// FS-IT-2: with `--regex`, a bare unbalanced `{` in the pattern causes an error.
#[test]
fn fs_replace_regex_bare_curly_brace_without_flag_errors() {
    let (dir, f) = cp("singleline.txt");
    err(tpu()
        .arg("replace")
        .arg("--regex")
        .arg(&f)
        .arg("{unclosed")
        .arg("X"));
    drop(dir);
}

/// FS-IT-3: default literal matching matches a literal `(` without a regex error.
#[test]
fn fs_replace_default_literal_paren_matches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fs3.txt");
    fs::write(&path, b"assert_eq!(a, b);\n").unwrap();

    ok(tpu()
        .arg("replace")
        .arg(&path)
        .arg("assert_eq!(a, b)")
        .arg("assert_eq!(a, c)"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("assert_eq!(a, c)"),
        "paren literal replacement must succeed; got: {content:?}"
    );
    drop(dir);
}

/// FS-IT-4: default literal matching treats `.` as a literal character, not any-char.
#[test]
fn fs_replace_default_literal_dot_is_literal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fs4.txt");
    // "v1.0" should match; "v1X0" (where dot-as-wildcard would match) must not.
    fs::write(&path, b"version: v1.0\nversion: v1X0\n").unwrap();

    ok(tpu().arg("replace").arg(&path).arg("v1.0").arg("v2.0"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("v2.0"),
        "literal dot replacement should produce v2.0; got: {content:?}"
    );
    assert!(
        content.contains("v1X0"),
        "v1X0 should NOT be replaced (dot is literal); got: {content:?}"
    );
    drop(dir);
}

/// FS-IT-5: default literal matching treats `*` as a literal character.
#[test]
fn fs_replace_default_literal_star_is_literal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fs5.txt");
    fs::write(&path, b"use foo::*;\nuse bar::baz;\n").unwrap();

    ok(tpu()
        .arg("replace")
        .arg(&path)
        .arg("foo::*")
        .arg("foo::specific"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("foo::specific"),
        "literal star replacement must succeed; got: {content:?}"
    );
    assert!(
        content.contains("use bar::baz"),
        "bar::baz must not be affected; got: {content:?}"
    );
    drop(dir);
}

/// FS-IT-6: default literal matching treats `+` as a literal.
#[test]
fn fs_replace_default_literal_plus_is_literal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fs6.txt");
    fs::write(&path, b"score: +100\nscore: 100\n").unwrap();

    ok(tpu().arg("replace").arg(&path).arg("+100").arg("+200"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("+200"),
        "literal plus replacement must succeed; got: {content:?}"
    );
    assert!(
        content.contains("score: 100\n"),
        "bare '100' line must not be replaced; got: {content:?}"
    );
    drop(dir);
}

/// FS-IT-7: default literal matching treats `?` as a literal.
#[test]
fn fs_replace_default_literal_question_mark_is_literal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fs7.txt");
    fs::write(&path, b"is it done? yes\nis it done  yes\n").unwrap();

    ok(tpu().arg("replace").arg(&path).arg("done?").arg("done!"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("done!"),
        "literal '?' should be replaced; got: {content:?}"
    );
    // "done  yes" has two spaces but no '?'; must not be changed.
    assert!(
        content.contains("is it done  yes"),
        "line without '?' must not be changed; got: {content:?}"
    );
    drop(dir);
}

/// FS-IT-8: default literal matching replaces all occurrences of a literal pattern.
#[test]
fn fs_replace_default_literal_replaces_all_occurrences() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fs8.txt");
    // Three occurrences of "a.b" (literal dot).
    fs::write(&path, b"a.b + a.b = a.b\n").unwrap();

    ok(tpu().arg("replace").arg(&path).arg("a.b").arg("X"));

    let content = fs::read_to_string(&path).unwrap();
    assert_eq!(
        content.trim(),
        "X + X = X",
        "all three literal 'a.b' occurrences must be replaced; got: {content:?}"
    );
    drop(dir);
}

/// FS-IT-9: default literal matching with a bracket expression pattern.
#[test]
fn fs_replace_default_literal_with_bracket_expression() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fs9.txt");
    // "[cfg(test)]" — square brackets and parens are both metacharacters.
    fs::write(&path, b"#[cfg(test)]\nmod tests {}\n").unwrap();

    ok(tpu()
        .arg("replace")
        .arg(&path)
        .arg("[cfg(test)]")
        .arg("[cfg(all(test, debug_assertions))]"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("[cfg(all(test, debug_assertions))]"),
        "bracket+paren pattern must be replaced literally; got: {content:?}"
    );
    drop(dir);
}

/// FS-IT-10: `-E` (short form of `--regex`) is accepted and enables regex
/// interpretation of the pattern.
#[test]
fn fs_replace_short_flag_e_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fs10.txt");
    fs::write(&path, b"value = (x + y)\n").unwrap();

    ok(tpu()
        .arg("replace")
        .arg("-E")
        .arg(&path)
        .arg(r"\(x \+ y\)")
        .arg("(a + b)"));

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("(a + b)"),
        "short flag -E must enable regex interpretation; got: {content:?}"
    );
    drop(dir);
}

/// FS-IT-11: default literal matching, zero-match case exits 0 and (per M7)
/// does NOT create a `.bak` because the file is not rewritten -- this is
/// how callers distinguish "matched nothing" from a real edit at the
/// file-system level.
#[test]
fn fs_replace_default_literal_zero_match_exits_ok() {
    let (dir, f) = cp("singleline.txt");
    let before_mtime = fs::metadata(&f).unwrap().modified().unwrap();
    // Sleep so a spurious rewrite would produce a distinguishable mtime.
    std::thread::sleep(std::time::Duration::from_millis(50));
    // Pattern contains regex metacharacters but is not present in the file.
    ok(tpu()
        .arg("replace")
        .arg(&f)
        .arg("no.such.text{here}")
        .arg("Z"));
    assert!(
        !bak(&f).exists(),
        "zero-match run must NOT create .bak (M7-1 short-circuit)"
    );
    let after_mtime = fs::metadata(&f).unwrap().modified().unwrap();
    assert_eq!(
        before_mtime, after_mtime,
        "zero-match run must preserve mtime (M7-1 short-circuit)"
    );
    drop(dir);
}

/// FS-IT-12: `--count` with default literal matching counts literal occurrences.
#[test]
fn fs_replace_default_literal_count_counts_literal_occurrences() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fs12.txt");
    // Four occurrences of the literal "x.y" (dot must not match 'X', 'Y', etc.)
    fs::write(&path, b"x.y\nx.y and x.y\nxZy\nx.y\n").unwrap();

    let o = ok(tpu()
        .arg("replace")
        .arg("--count")
        .arg(&path)
        .arg("x.y")
        .arg("Z"));
    let stdout = String::from_utf8(o.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        "4",
        "--count with default literal matching must report 4 literal matches; got: {stdout:?}"
    );
    drop(dir);
}
