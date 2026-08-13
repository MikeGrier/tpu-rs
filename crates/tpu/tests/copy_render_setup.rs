// Copyright (c) 2026, Michael Grier
//! Integration tests for the new `copy`, `render`, and `setup` subcommands
//! plus the global `--on-error warn|fail` flag for tree-walking commands.

use std::{
    fs,
    path::Path,
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

    ok(tpu().arg("copy").arg("--recursive").arg(&src).arg(&dst));
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

#[test]
fn copy_glob_copies_matching_files_flat_into_dest() {
    let dir = TempDir::new().unwrap();
    let src_dir = dir.path().join("src");
    let dest_dir = dir.path().join("dest");
    write_file(&src_dir.join("a.txt"), b"A");
    write_file(&src_dir.join("b.txt"), b"B");
    write_file(&src_dir.join("skip.bin"), b"BINARY");

    // Pass an absolute-path glob so the pattern is unambiguous regardless
    // of the test process's working directory.
    let pattern = format!("{}/*.txt", src_dir.display());
    ok(tpu().arg("copy").arg(&pattern).arg(&dest_dir));

    assert_eq!(fs::read(dest_dir.join("a.txt")).unwrap(), b"A");
    assert_eq!(fs::read(dest_dir.join("b.txt")).unwrap(), b"B");
    // Non-matching file must not be copied.
    assert!(
        !dest_dir.join("skip.bin").exists(),
        "skip.bin must not be in dest"
    );
}

#[test]
fn copy_glob_no_match_errors() {
    let dir = TempDir::new().unwrap();
    let dest_dir = dir.path().join("dest");
    let pattern = format!("{}/*.nonexistent", dir.path().display());

    let o = tpu()
        .arg("copy")
        .arg(&pattern)
        .arg(&dest_dir)
        .output()
        .unwrap();
    assert!(
        !o.status.success(),
        "expected non-zero exit for no-match glob"
    );
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("matched no files"),
        "expected 'matched no files' in stderr; got: {stderr}"
    );
    assert!(
        !dest_dir.exists(),
        "destination directory should not be created when glob has no matches"
    );
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
        .arg("--var")
        .arg("NAME=World")
        .arg("--var")
        .arg("DAY=Friday"));
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
        .arg("--var")
        .arg("NAME=ok"));
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
        .arg("--template")
        .arg("hi {{NAME}}")
        .arg("--missing")
        .arg("empty"));
    assert_eq!(fs::read_to_string(&out).unwrap(), "hi ");
}

#[test]
fn render_missing_leave_keeps_placeholder() {
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("out.txt");
    ok(tpu()
        .arg("render")
        .arg(&out)
        .arg("--template")
        .arg("hi {{NAME}}")
        .arg("--missing")
        .arg("leave"));
    assert_eq!(fs::read_to_string(&out).unwrap(), "hi {{NAME}}");
}

// ─── setup ───────────────────────────────────────────────────────────────────

#[test]
fn setup_print_emits_marker_block() {
    let out = ok(tpu().arg("setup")).stdout;
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("<!-- tpu-mcp:setup:begin -->"),
        "begin marker missing: {s}"
    );
    assert!(
        s.contains("<!-- tpu-mcp:setup:end -->"),
        "end marker missing"
    );
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

/// M8-5: the injected guidance block records the version of `tpu` that
/// wrote it as an HTML comment on its first line, so callers can compare
/// it against the `tpu_version` field emitted in every `tpu_*` response's
/// invocation header (M8-1) and detect binary/guidance version drift.
/// Verified on both a fresh inject and a re-inject that replaces a stale
/// block, and on the plain-print (non-inject) path.
#[test]
fn setup_emits_version_marker_matching_cargo_pkg_version() {
    let expected = format!(
        "<!-- tpu-mcp:setup:version={} -->",
        env!("CARGO_PKG_VERSION")
    );

    // Plain print.
    let out = String::from_utf8_lossy(&ok(tpu().arg("setup")).stdout).into_owned();
    assert!(
        out.contains(&expected),
        "plain-print setup must include version marker {expected:?}; got:
{out}"
    );

    // Fresh inject.
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("fresh.md");
    ok(tpu().arg("setup").arg("--inject").arg(&target));
    let fresh = fs::read_to_string(&target).unwrap();
    assert!(
        fresh.contains(&expected),
        "fresh --inject must embed version marker {expected:?}; got:
{fresh}"
    );

    // Re-inject over a stale block (with an outdated version marker).
    let target2 = dir.path().join("stale.md");
    write_file(
        &target2,
        b"# Header

<!-- tpu-mcp:setup:begin -->
\
          <!-- tpu-mcp:setup:version=0.0.0-stale -->
stale body
\
          <!-- tpu-mcp:setup:end -->
trailer
",
    );
    ok(tpu().arg("setup").arg("--inject").arg(&target2));
    let refreshed = fs::read_to_string(&target2).unwrap();
    assert!(
        refreshed.contains(&expected),
        "re-inject must overwrite the stale version marker; got:
{refreshed}"
    );
    assert!(
        !refreshed.contains("0.0.0-stale"),
        "re-inject must remove the stale version marker; got:
{refreshed}"
    );
    assert!(
        refreshed.contains("trailer"),
        "trailing content preserved on re-inject"
    );
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
        .arg("--pattern")
        .arg("hit")
        .arg("--path")
        .arg(missing.to_str().unwrap())
        .arg("--path")
        .arg(real.to_str().unwrap())
        .output()
        .unwrap();
    assert!(out.status.success(), "warn mode should not fail");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hit me"),
        "match from real file expected: {stdout}"
    );
}

#[test]
fn find_fail_mode_aborts_on_missing_path() {
    let dir = TempDir::new().unwrap();
    let real = dir.path().join("real.txt");
    write_file(&real, b"hit me");
    let missing = dir.path().join("does_not_exist");

    let out = tpu()
        .arg("--on-error")
        .arg("fail")
        .arg("find")
        .arg("--pattern")
        .arg("hit")
        .arg("--path")
        .arg(missing.to_str().unwrap())
        .arg("--path")
        .arg(real.to_str().unwrap())
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "fail mode must abort on missing path"
    );
}

// ─── Windows-only: recursive copy through a DACL-denied subdirectory ────────

/// RAII guard that opens a directory with `WRITE_DAC | READ_CONTROL` (all
/// `FILE_SHARE_*` bits set so the handle survives the denial period), then:
///
/// 1. snapshots the existing DACL via `GetSecurityInfo`;
/// 2. builds a new DACL with a single `FILE_LIST_DIRECTORY` deny ACE for the
///    current process's user SID;
/// 3. applies that DACL with `SetSecurityInfo`; and
/// 4. on drop, restores the original DACL through the still-open handle and
///    frees the snapshot with `LocalFree`.
///
/// Declare this guard *after* `TempDir` in each test so that Rust's
/// reverse-drop order restores the DACL before `TempDir` tries to remove the
/// directory tree.
#[cfg(windows)]
struct DaclDenyGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
    orig_dacl: *mut windows_sys::Win32::Security::ACL,
    /// Security-descriptor storage returned by `GetSecurityInfo`; `orig_dacl`
    /// points into this allocation, so it must outlive every use of `orig_dacl`.
    sd: *mut core::ffi::c_void,
}

// The raw pointers are exclusively owned by this guard and are never shared
// across threads.
#[cfg(windows)]
unsafe impl Send for DaclDenyGuard {}

#[cfg(windows)]
impl DaclDenyGuard {
    /// Open `dir` with `WRITE_DAC`, snapshot its DACL, then apply a
    /// `FILE_LIST_DIRECTORY` deny ACE for the current user.
    fn deny_listing(dir: &std::path::Path) -> Self {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::{
            Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
            Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo},
            Security::{
                ACL, ACL_REVISION, AddAccessDeniedAce, DACL_SECURITY_INFORMATION, GetLengthSid,
                GetTokenInformation, InitializeAcl, TOKEN_QUERY, TOKEN_USER, TokenUser,
            },
            Storage::FileSystem::{
                CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
                FILE_SHARE_WRITE, OPEN_EXISTING,
            },
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        };

        unsafe {
            // Null-terminate the directory path as UTF-16.
            let wide: Vec<u16> = dir
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0u16))
                .collect();

            // WRITE_DAC = 0x00040000, READ_CONTROL = 0x00020000.  These are
            // standard object access rights that windows-sys 0.61 does not
            // expose as named constants separate from their typed variants.
            const WRITE_DAC: u32 = 0x0004_0000;
            const READ_CONTROL: u32 = 0x0002_0000;

            // Open with WRITE_DAC so restoration works even after we deny our
            // own read access.  FILE_FLAG_BACKUP_SEMANTICS is required to open
            // a directory handle.  All three FILE_SHARE_* bits prevent the
            // open from failing if something else holds the directory.
            let handle = CreateFileW(
                wide.as_ptr(),
                WRITE_DAC | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(), // hTemplateFile
            );
            assert_ne!(handle, INVALID_HANDLE_VALUE, "CreateFileW({dir:?}) failed");

            // Snapshot the existing DACL.  `orig_dacl` points inside `sd`;
            // both must remain alive until SetSecurityInfo is called in Drop.
            let mut orig_dacl: *mut ACL = std::ptr::null_mut();
            let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
            assert_eq!(
                GetSecurityInfo(
                    handle,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut orig_dacl,
                    std::ptr::null_mut(),
                    &mut sd,
                ),
                0,
                "GetSecurityInfo failed"
            );

            // Obtain the current user's SID from the process token.
            let mut token: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
            assert_ne!(
                OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
                0,
                "OpenProcessToken failed"
            );
            let mut needed: u32 = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            let mut token_buf = vec![0u64; (needed as usize + 7) / 8];
            assert_ne!(
                GetTokenInformation(
                    token,
                    TokenUser,
                    token_buf.as_mut_ptr().cast(),
                    needed,
                    &mut needed,
                ),
                0,
                "GetTokenInformation failed"
            );
            let _ = CloseHandle(token);

            let user_sid = (*(token_buf.as_ptr() as *const TOKEN_USER)).User.Sid;
            let sid_len = GetLengthSid(user_sid);

            // Build a one-ACE deny DACL.
            // Memory layout: 8-byte ACL header
            //               + 8 bytes (ACE_HEADER + Mask)
            //               + full SID body (replaces the inline SidStart DWORD).
            let acl_len: u32 = 8 + 8 + sid_len;
            let mut acl_buf = vec![0u64; (acl_len as usize + 7) / 8];
            let acl_ptr = acl_buf.as_mut_ptr() as *mut ACL;
            assert_ne!(
                InitializeAcl(acl_ptr, acl_len, ACL_REVISION as u32),
                0,
                "InitializeAcl failed"
            );
            // FILE_LIST_DIRECTORY (0x0001) prevents NtQueryDirectoryFile /
            // FindNextFileW from succeeding when the caller tries to enumerate
            // the directory contents.
            const FILE_LIST_DIRECTORY: u32 = 0x0001;
            assert_ne!(
                AddAccessDeniedAce(acl_ptr, ACL_REVISION as u32, FILE_LIST_DIRECTORY, user_sid),
                0,
                "AddAccessDeniedAce failed"
            );

            // Apply the deny DACL.  Subsequent opens of `dir` for
            // FILE_LIST_DIRECTORY will receive ERROR_ACCESS_DENIED.
            assert_eq!(
                SetSecurityInfo(
                    handle,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    acl_ptr,
                    std::ptr::null_mut(),
                ),
                0,
                "SetSecurityInfo (apply deny) failed"
            );

            DaclDenyGuard {
                handle,
                orig_dacl,
                sd,
            }
        }
    }
}

#[cfg(windows)]
impl Drop for DaclDenyGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, LocalFree},
            Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo},
            Security::DACL_SECURITY_INFORMATION,
        };
        unsafe {
            // Restore the original DACL through the still-open WRITE_DAC handle.
            let _ = SetSecurityInfo(
                self.handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                self.orig_dacl, // null restores a NULL DACL (grant-all) when appropriate
                std::ptr::null_mut(),
            );
            // Release the security-descriptor buffer returned by GetSecurityInfo.
            let _ = LocalFree(self.sd);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Tree used by both DACL-based copy tests:
///
/// ```text
/// <tmp>/
///   a/              ← source root passed to `tpu copy --recursive`
///     b/            ← FILE_LIST_DIRECTORY denied for the current principal
///       c/
///         secret.txt
///     d/            ← peer of b; successfully copied in warn mode
///       found.txt
///   dst/            ← copy destination
/// ```
#[test]
#[cfg(windows)]
fn copy_recursive_warn_mode_continues_past_denied_subdir() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a");
    write_file(&a.join("b").join("c").join("secret.txt"), b"secret");
    write_file(&a.join("d").join("found.txt"), b"found");
    let dst = dir.path().join("dst");

    // `_guard` is declared after `dir`, so it drops first (reverse-drop order),
    // restoring the DACL before TempDir attempts to clean up the tree.
    let _guard = DaclDenyGuard::deny_listing(&a.join("b"));

    // Sanity-check: confirm the DACL deny actually took effect on this runner.
    // Some CI environments (e.g. ones with SeBackupPrivilege) may bypass DACLs;
    // return early with a clear diagnostic rather than failing confusingly.
    match std::fs::read_dir(a.join("b")) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
        probe => {
            eprintln!(
                "DACL deny did not produce PermissionDenied on this runner; \
                 skipping test (probe: {probe:?})"
            );
            return;
        }
    }

    let out = tpu()
        .arg("--on-error")
        .arg("warn")
        .arg("copy")
        .arg("--recursive")
        .arg(&a)
        .arg(&dst)
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "warn mode should exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The peer directory d must have been copied despite the failure on b.
    assert!(
        dst.join("d").join("found.txt").exists(),
        "d/found.txt must be present in dst; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The denied subtree must not appear in dst.
    assert!(
        !dst.join("b").join("c").join("secret.txt").exists(),
        "secret.txt must not be copied from the denied subtree"
    );
    // A warning about the inaccessible path must appear on stderr.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let denied_dir = a.join("b");
    let denied_str = denied_dir.to_string_lossy();
    assert!(
        stderr.to_ascii_lowercase().contains("warn") || stderr.contains(denied_str.as_ref()),
        "expected a warning mentioning the denied path; stderr: {stderr}"
    );
}

#[test]
#[cfg(windows)]
fn copy_recursive_fail_mode_aborts_on_denied_subdir() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a");
    write_file(&a.join("b").join("c").join("secret.txt"), b"secret");
    write_file(&a.join("d").join("found.txt"), b"found");
    let dst = dir.path().join("dst");

    let _guard = DaclDenyGuard::deny_listing(&a.join("b"));

    // Sanity-check: confirm the DACL deny actually took effect on this runner.
    match std::fs::read_dir(a.join("b")) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
        probe => {
            eprintln!(
                "DACL deny did not produce PermissionDenied on this runner; \
                 skipping test (probe: {probe:?})"
            );
            return;
        }
    }

    let out = tpu()
        .arg("--on-error")
        .arg("fail")
        .arg("copy")
        .arg("--recursive")
        .arg(&a)
        .arg(&dst)
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "fail mode must exit non-zero when a subdirectory is inaccessible;\
         \nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
