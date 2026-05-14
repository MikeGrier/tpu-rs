// Copyright (c) 2026, Michael Grier

//! End-to-end integration test for the corruption doom-loop.
//!
//! This is the canonical demonstration that the historical failure mode
//! (a misbehaving caller silently propagating mojibake into a clean
//! file) can no longer happen.  The full six-step scenario:
//!
//! 1. Start with a clean UTF-8 file containing em-dashes and box-drawing.
//! 2. A library caller attempts to write mojibake'd bytes via the
//!    default policy → the write is rejected with
//!    [`tpu::mojibake::MojibakeIntroduced`] and the file is unchanged.
//! 3. Force-write the same bytes with [`WritePolicy::permissive`]
//!    (simulating the pre-fix world) → the file is now corrupt on disk.
//! 4. `tpu read` on the corrupt file emits a one-line `note:` advisory
//!    on stderr while still returning the file's bytes verbatim on
//!    stdout (read is never blocked).
//! 5. `tpu doctor --fix=peel` repairs the file in place.
//! 6. Re-run `tpu doctor --format=json` and assert the JSON report
//!    contains zero remaining issues.
//!
//! All literal mojibake byte sequences come from
//! [`tpu::test_fixtures`] (decoded from base-64 at runtime) so this
//! source file remains pure ASCII and needs no `allow-mojibake` opt-out.

use std::{
    fs,
    process::{Command, Output, Stdio},
};

use tempfile::TempDir;
use tpu::IoMode;
use tpu::cmd::doctor::{self, DoctorFix, DoctorFormat, DoctorOptions};
use tpu::mojibake::WritePolicy;
use tpu::test_fixtures::{box_drawing, em_dash};

fn tpu_cli() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tpu"));
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

fn run_ok(cmd: &mut Command) -> Output {
    let o = cmd.output().expect("spawn tpu");
    assert!(
        o.status.success(),
        "tpu exited non-zero: {}\nstdout: {}\nstderr: {}",
        o.status,
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr),
    );
    o
}

#[test]
fn corruption_doom_loop_is_broken_end_to_end() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("doc.txt");

    // ── (1) Clean seed: real em-dash plus real box-drawing characters.
    //         "intro \u{2014} divider \u{2500}\n"
    let clean = "intro \u{2014} divider \u{2500}\n";
    fs::write(&path, clean.as_bytes()).expect("write clean seed");
    assert_eq!(fs::read(&path).unwrap(), clean.as_bytes());

    // ── (2) Library caller attempts to inject mojibake under the
    //         default (strict) policy.  Must be rejected; file unchanged.
    let corrupt_text = format!("intro {} divider {}\n", em_dash(), box_drawing());
    let err = tpu::cmd::write::run(
        &path,
        &corrupt_text,
        tpu::encoding::OutputEncoding::Preserve,
        tpu::encoding::BomPolicy::default(),
        None,
        None,
        IoMode::Buffered,
        WritePolicy::default(),
    )
    .expect_err("strict policy must reject the mojibake injection");
    assert!(
        err.to_string().contains("introduce mojibake"),
        "error must explain rejection: {err}"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        clean.as_bytes(),
        "file must be untouched after rejected write"
    );

    // ── (3) Force-write the same content with the permissive policy.
    //         Simulates the pre-fix world; the file is now corrupt.
    tpu::cmd::write::run(
        &path,
        &corrupt_text,
        tpu::encoding::OutputEncoding::Preserve,
        tpu::encoding::BomPolicy::default(),
        None,
        None,
        IoMode::Buffered,
        WritePolicy::permissive(),
    )
    .expect("permissive policy must accept the mojibake content");
    let on_disk = fs::read(&path).expect("read corrupt file");
    assert_eq!(
        on_disk,
        corrupt_text.as_bytes(),
        "file should now contain the mojibake bytes verbatim"
    );
    // Clean up the .bak file the atomic-write codepath produced.
    let _ = fs::remove_file(format!("{}.bak", path.display()));

    // ── (4) `tpu read` on the corrupt file: stdout = bytes verbatim,
    //         stderr contains exactly one mojibake `note:` advisory,
    //         exit code 0.
    let o = run_ok(tpu_cli().arg("read").arg(&path));
    assert_eq!(
        o.stdout,
        corrupt_text.as_bytes(),
        "stdout must be unchanged"
    );
    let stderr = String::from_utf8_lossy(&o.stderr);
    let advisory_lines: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with("note: ") && l.contains("mojibake"))
        .collect();
    assert_eq!(
        advisory_lines.len(),
        1,
        "expected exactly one advisory line; stderr was: {stderr:?}"
    );
    assert!(advisory_lines[0].contains("run 'tpu doctor' for details"));

    // ── (5) `tpu doctor --fix=peel` repairs the file in place.  We
    //         invoke the library `run` directly so we can introspect
    //         the report; the CLI surface is exercised in the doctor
    //         integration test.
    {
        let mut sink = Vec::new();
        let path_str = path.to_string_lossy();
        let report = doctor::run(
            &[path_str.as_ref()],
            DoctorOptions {
                format: DoctorFormat::Human,
                fix: DoctorFix::Peel,
                quiet: true,
            },
            &mut sink,
            IoMode::Buffered,
        )
        .expect("doctor --fix=peel must succeed");
        assert_eq!(
            report.total_repaired, 1,
            "doctor must report exactly one file repaired"
        );
    }
    // After the peel, the file should match the clean seed bytes again.
    assert_eq!(
        fs::read(&path).unwrap(),
        clean.as_bytes(),
        "peel must restore the original clean UTF-8 bytes"
    );

    // ── (6) Re-scan: the repaired file should have zero remaining issues.
    {
        let mut sink = Vec::new();
        let path_str = path.to_string_lossy();
        let report = doctor::run(
            &[path_str.as_ref()],
            DoctorOptions {
                format: DoctorFormat::Json,
                fix: DoctorFix::None,
                quiet: false,
            },
            &mut sink,
            IoMode::Buffered,
        )
        .expect("re-scan must succeed");
        assert_eq!(
            report.total_issues(),
            0,
            "post-repair scan must report zero issues; report: {report:?}"
        );
    }
}
