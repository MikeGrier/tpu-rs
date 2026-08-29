<!-- Copyright (c) 2026, Michael Grier -->
<!-- encoding-check: allow-mojibake (this file documents mojibake detection) -->

# Mojibake / encoding-corruption detection

Detect — and ultimately refuse to introduce — text-encoding corruption inside
the `tpu` library itself. Builds on the external `tools/check-encoding.ps1` /
CI guard by moving the checks one layer closer to the source of corruption.

The corruption pattern this targets: UTF-8 bytes that were decoded through a
single-byte code page (Windows-1252) and then re-saved as UTF-8, producing
multibyte sequences like `Ã©`, `â€"`, `â"€`, `Â<NBSP>`. PowerShell `Get-Content`
/ `Set-Content` are the historical culprits.

---

## Milestone 1 — Detection primitives

**Theme:** a single, reusable library module that answers the question
*"does this decoded text look like it was mojibake'd?"* with no I/O.

- [x] M1-1: New module `tpu::mojibake` with `pub fn scan(text: &str) -> ScanReport`
      returning `{ matches: Vec<Match>, total_chars: usize }` where each
      `Match` has byte offset + pattern name. Patterns: Latin-1 prefix
      (`\u{C3}[\u{80}-\u{BF}]`), punctuation prefix (`\u{E2}\u{20AC}…`),
      box-draw prefix (`\u{E2}\u{201D}…`), NBSP-as-`Â<sp>` (`\u{C2}\u{A0}`).
- [x] M1-2: `pub fn first_match(text: &str) -> Option<Match>` short-circuit
      variant for hot paths.
- [x] M1-3: `pub fn allowed_by_marker(text: &str) -> bool` — recognise the
      same `encoding-check: allow-mojibake` opt-out sentinel that the
      PowerShell guard uses, so library callers can honour explicit
      documentation files.
- [x] M1-4: `pub fn looks_like_one_layer_peel(text: &str) -> Option<String>`
      — apply one round of "decode-as-UTF-8 → re-encode any single-byte
      Win1252 char as that byte → re-decode as UTF-8" and return the
      result *only* if it has strictly fewer mojibake matches than the
      input. Used by the doctor command to suggest a likely recovery.

**Integration test (theme):** `tests/mojibake_detection.rs` — 8 tests
over a 9-fixture corpus (clean ASCII; clean UTF-8 with em-dashes /
box-drawing / CJK / emoji; each pattern in isolation; all four mixed;
doubly-mojibake; allow-marker opt-out) verify that scan / first_match
/ allowed_by_marker / peel agree end-to-end.  Lock-down tests for
`ALLOW_MARKER`, `Pattern::name()`, and a panic-resistance smoke test
over pathological inputs round it out.

**Status:** 26 unit tests + 8 integration tests, 0 failures.

---

## Milestone 2 — Write-time guard (the keystone)

**Theme:** the `tpu` library refuses, by default, to write bytes that
*introduce* mojibake compared to the file's prior content. A future
miswritten replacement / edit fails fast at the library boundary instead
of being saved.

- [x] M2-1: `pub struct WritePolicy { pub reject_introduced_mojibake: bool }`
      with `Default::default()` enabling the check.  Plumbed through
      `cmd::write::run`, `cmd::replace::run`, `cmd::edit::run`,
      `cmd::append::run` as a new trailing parameter.
- [x] M2-2: Helper `pub fn check_write_does_not_introduce_mojibake(old, new)
      -> Result<(), MojibakeIntroduced>` returning the per-pattern *new*
      matches, the first introduced match, and a `Display` impl that
      produces the standard `"writing this content would introduce
      mojibake (… 'latin1' at byte offset N); pass --allow-mojibake to
      override"` hint.  Honours `ALLOW_MARKER` in the new content.
- [x] M2-3: Wired into all four call sites.  Replace / write / edit
      decode old + new bytes through the file's detected encoding so
      the check is performed in UTF-8 char space; append uses the
      already-decoded text.  Atomic-write semantics ensure rejected
      writes leave the file unmodified.  Binary mode (`tpu edit -b`,
      `tpu write --binary`) skips the check by design.
- [x] M2-4: CLI flag `--allow-mojibake` on `write`, `replace`, `edit`,
      `append` (and matching `allow_mojibake` JSON arg accepted by
      `tpu_write_file` / `tpu_replace_in_file` / `tpu_edit_file` /
      `tpu_append_file` in `tpu-mcp`) flips the policy to permissive.

**Integration test (theme):** `tests/write_guard.rs` — seven scenarios
(the five from the original spec plus two extras locking down the
per-pattern budget logic and the brand-new-file allow-marker path):

1. `replace::run` injecting `Ã©` is rejected; file untouched.
2. Same with `WritePolicy::permissive()` succeeds; file updated.
3. `write::run` overwriting a file containing `â€"` with identical
   bytes succeeds (no *new* corruption).
4. `edit::run` splice that *removes* a region of mojibake succeeds.
5. `append::run` adding clean content to a file with pre-existing
   mojibake succeeds.
6. `write::run` adding a *second* `Ã¨` to a file already containing
   one `Ã©` is rejected (set-difference semantics).
7. `write::run` to a brand-new file containing the allow-marker
   succeeds even with mojibake (per-content opt-out).

**Status:** 41 unit tests + 7 integration tests, 0 failures.  Full
workspace remains green at 2 806 tests across all crates.

---

## Milestone 3 — `tpu doctor` subcommand

**Theme:** a one-stop diagnostic that scans paths / globs, reports
encoding problems with byte-precise locations, and suggests recovery
where possible. Uses harrier for accurate non-UTF-8 detection too.

- [x] M3-1: New subcommand `tpu doctor [paths...]` (default `.`) with
      `--format=human|json`, `--fix=peel`, `--quiet`/`-q`.  Walks files
      via `walkdir`, expands shell-style globs (`*`, `?`, `[`, `{`)
      via `globset`, honours a top-level `.gitignore` (basic
      non-negation patterns), and skips `.git/`, `node_modules/`,
      `target/`, plus a curated list of binary file extensions.
- [x] M3-2: Per-file [`DoctorIssue`] record:
      `{ path, encoding_detected, valid_in_detected_encoding,
      mojibake_matches: [{byte_offset, line, col, pattern}],
      peel_suggested, repaired }`.  Decoding goes through harrier so
      non-UTF-8 files (UTF-16, Win-1252) are checked in their *own*
      encoding.  Encoding-invalid files skip the mojibake scan to
      avoid replacement-character false positives.  Files containing
      [`mojibake::ALLOW_MARKER`] are reported clean.
- [x] M3-3: `--fix=peel` mode applies
      [`mojibake::looks_like_one_layer_peel`] to each flagged file;
      strictly-better peels are written via [`cmd::write::run`] with
      [`mojibake::WritePolicy::permissive`] so the existing `.bak`
      machinery and atomic-write path apply uniformly.  Files where
      peel doesn't help are left alone and reported.

**Integration test (theme):** `tests/doctor.rs` — three end-to-end
scenarios over a seven-fixture temp tree (clean ASCII, clean UTF-8,
UTF-16LE-with-BOM, single-mojibake, mixed-pattern mojibake,
encoding-invalid UTF-8, allow-marker file):

1. Plain run flags exactly the corrupt and encoding-invalid files;
   clean / marker fixtures are untouched; repair count is zero.
2. `--format=json` produces a parseable JSON document with the
   documented per-file schema (`path`, `encoding_detected`,
   `valid_in_detected_encoding`, `mojibake_matches[]` with
   `{byte_offset, line, col, pattern}`, `peel_suggested`, `repaired`)
   and top-level totals.
3. `--fix=peel` repairs the single-mojibake file; the encoding-invalid
   file is left byte-identical on disk; the rescan shows the
   single-mojibake file no longer flagged (or strictly fewer matches)
   while the encoding-invalid file remains flagged.

**Status:** 17 unit tests + 3 integration tests, 0 failures.  Full
workspace remains green at 2 843 tests across all crates.

---

## Milestone 4 — Read-time advisory

**Theme:** when `tpu` decodes a file that already contains mojibake, it
emits a one-line stderr note so callers (especially LLM agents) realise
*the file was already broken*, rather than blaming the previous tool.
Read operations are never blocked.

- [x] M4-1: In `cmd::read::run`, `cmd::head::run`, `cmd::tail::run`,
      `cmd::readex::run`, after decoding call `mojibake::first_match`
      on the decoded text; if `Some`, emit
      `note: <path>: file appears to contain mojibake (N matches); run
      'tpu doctor' for details` to the run's stderr writer (not
      `eprintln!`, so MCP captures it).
      - **Unit tests** (per command): clean file produces no note;
        mojibake'd file produces exactly one note containing the count
        and the path; allow-marker suppresses the note; note goes to
        the configured stderr writer, not stdout (prevents output
        contamination).
- [x] M4-2: Global `--no-mojibake-warning` flag (and matching env var
      `TPU_NO_MOJIBAKE_WARNING=1`) for users who run on known-corrupt
      corpora and don't want noise.
      - **Unit tests:** flag suppresses the note, env var suppresses
        the note, both off → note appears, both on → still suppressed
        (no double-negative bug).

**Integration test (theme):** `tests/read_advisory.rs` — three
scenarios:
1. `tpu read clean.txt` produces no stderr.
2. `tpu read corrupt.txt` produces exactly one `note: …` line on
   stderr, normal content on stdout, exit code 0.
3. `tpu read corrupt.txt --no-mojibake-warning` matches scenario 1's
   stderr behaviour (silent).

**Status:** ✅ Complete. The read-time advisory is implemented as a
pure decision helper (`mojibake::check_read_advisory`) plus a writer
helper (`mojibake::emit_read_advisory`); both are covered by 7
mojibake-module unit tests.  All four read-side commands accept a new
trailing `notes: Option<&mut dyn Write>` parameter and call the
helper after decoding (advisory is skipped automatically in `head
--bytes` / `tail --bytes` modes since they never decode).  The CLI
honours `--no-mojibake-warning` and `TPU_NO_MOJIBAKE_WARNING=1`, and
auto-suppresses the advisory whenever `--message-format=json` is
active to avoid corrupting the NDJSON stream.  9 integration tests
in `tests/read_advisory.rs` cover all five command surfaces (read,
readex, head, tail) plus JSON-mode containment, allow-marker
suppression, env-var suppression, and the global flag.  Workspace
test count is 2 864 (up from 2 843 baseline; +7 mojibake unit tests,
+9 integration tests, +5 from re-runs of existing modules).
Encoding sweep clean (every modified `.rs` file is valid UTF-8 with
no mojibake digraphs).

---

## Milestone 5 — Documentation + integration

**Theme:** the new capabilities are discoverable and the existing
external guard knows about them.

- [x] M5-1: Update `crates/tpu/src/cmd/{write,replace,edit,append}.rs`
      module docs to describe the write-time guard and the override
      flag.
- [x] M5-2: Update `crates/tpu-mcp/src/tools.rs` schemas: add
      `allow_mojibake` boolean to the four mutating tools; document
      the read-time advisory in their descriptions.
- [x] M5-3: Update `.github/copilot-instructions.md` "Recovering from
      observed tool failures" section to mention `tpu doctor` as the
      first-line diagnostic when corruption is suspected.
- [x] M5-4: Update `tools/check-encoding.ps1` header comment to point
      callers at `tpu doctor` for richer output and recovery.

**Integration test (theme):** `tests/end_to_end_corruption_loop.rs` —
reproduces the historical failure mode and proves it can no longer
silently propagate:
1. Start with a clean UTF-8 file containing em-dashes and box-drawing.
2. Simulate a misbehaving caller writing mojibake'd bytes via the
   `tpu` library → write is rejected, file unchanged.
3. Force-write the same bytes with `--allow-mojibake` (simulating the
   pre-fix world) → file now corrupt.
4. `tpu read` on the corrupt file emits the advisory note.
5. `tpu doctor --fix=peel` repairs the file.
6. Re-scan: clean.

This test is the canonical demonstration that the corruption doom-loop
is broken end-to-end.

**Status:** ✅ Complete.  All four documentation tasks landed: the
four mutating-command modules (`write`, `replace`, `edit`, `append`)
gained a "Write-time mojibake guard" doc section explaining the
guard, the `WritePolicy::permissive` / `--allow-mojibake` /
`"allow_mojibake": true` overrides, and the `ALLOW_MARKER`
suppression; the four MCP tool schemas in `crates/tpu-mcp/src/tools.rs`
gained an `allow_mojibake` boolean property with descriptions
mirroring the CLI semantics, and the `tpu_read_file` tool's
description now points callers at `tpu doctor` when mojibake is
suspected; `.github/copilot-instructions.md` now teaches `tpu doctor
<path>` (with `--fix=peel` and `--format=json`) as the first-line
diagnostic, plus the read-time advisory + `--no-mojibake-warning` /
`TPU_NO_MOJIBAKE_WARNING=1` and the write-time guard + override
flags; and `tools/check-encoding.ps1` now points callers at
`tpu doctor` for richer per-file output and in-place repair, framing
itself as the cheap binary gate.  The canonical end-to-end
integration test `tests/end_to_end_corruption_loop.rs` reproduces
the full doom-loop scenario in one run: clean seed → library write
rejected with `MojibakeIntroduced` → force-write with
`WritePolicy::permissive()` succeeds → `tpu read` emits the advisory
while still returning content verbatim → `tpu doctor --fix=peel`
repairs the file → re-scan with `--format=json` reports
`total_issues == 0`.  Workspace test count is 2 874 (up from 2 864
baseline; +1 end-to-end integration test, +9 base-64 fixture
self-tests in `tpu::test_fixtures`).  Encoding sweep clean on
every modified path (`crates/tpu/src`, `crates/tpu-mcp/src`,
`crates/tpu/tests`, `.github`, `tools`).  Every refactored test and
source file is now pure ASCII: literal mojibake byte sequences live
exclusively in `tpu::test_fixtures` (decoded from base-64 at
runtime), so no `// encoding-check: allow-mojibake` opt-out marker is
needed in the four `cmd::{write,replace,edit,append}` modules, in
`tpu-mcp::tools`, or in any of the four refactored integration tests
(`write_guard`, `read_advisory`, `mojibake_detection`, `doctor`).

---

## Milestone 6 — U+FFFD residue detection (`tpu doctor --guess`)

**Theme:** a second, categorically distinct diagnostic class for `tpu doctor`.
While peelable mojibake is a *mechanical* encoding mismatch with a known
algorithmic undo, a `U+FFFD` replacement character (`EF BF BD`) in otherwise
valid UTF-8 is a *terminal loss* — the original codepoint is gone and no
mechanical reversal exists.  Context inference is the only route to recovery,
which means a human (or LLM with document context) must approve every fix.
`tpu doctor` therefore *reports* these but never auto-repairs them.

**Real-world motivation (firebird repo, June 2026):** 113 `U+FFFD` occurrences
across 7 markdown docs were repaired by context inference.  The corruption was
selective, not wholesale — surviving valid-UTF-8 em-dashes coexisted with
`U+FFFD` residue in the same files, and the lost characters spanned multiple
distinct source codepoints (`—` U+2014, `–` U+2013, `×` U+00D7).  Git history
didn't help (already `U+FFFD` at first commit).  This confirmed that
(a) `U+FFFD` recovery is inherently per-occurrence and context-dependent, and
(b) `tpu doctor` must *report* but never auto-fix these.

- [x] M6-1: `mojibake::scan_replacement_chars(text, guess) -> Vec<ReplacementCharMatch>`
      scans for `U+FFFD` in otherwise-valid UTF-8; each match carries
      `{ byte_offset, context }` (20-char window).  With `guess: true`,
      calls the private `guess_replacement_char` heuristic:
      space-flanked → em-dash `—` (U+2014); digit-flanked → en-dash `–`
      (U+2013); otherwise `None`.
- [x] M6-2: `mojibake::ALLOW_REPLACEMENT_CHAR_MARKER` / `has_replacement_char_allow_marker`
      — new opt-out sentinel `encoding-check: allow-replacement-char` that
      suppresses U+FFFD detection only (ordinary `allow-mojibake` suppresses
      everything; the new marker suppresses only the replacement-char scan,
      so a file can still be flagged for peelable mojibake).
- [x] M6-3: `DoctorIssue` gains `replacement_char_matches: Vec<DoctorReplacementCharMatch>`
      where each entry has `{ byte_offset, line, col, context, suggested: Option<char> }`.
      `DoctorIssue::is_problem()` now also fires when this vec is non-empty.
      `peel_suggested` is never set for replacement-char-only files.
- [x] M6-4: `DoctorOptions` gains `pub guess: bool` (default `false`); the
      CLI exposes it as `--guess`.  Matching `guess: false` default added to
      the MCP `call_doctor` path in `tpu-mcp/src/tools.rs`.
- [x] M6-5: Human output: replacement-char section shown when `rc_count > 0`,
      format `"  LINE:COL: [lossy-replacement] (byte offset N)"` with optional
      `"(suggest: U+XXXX 'CHAR')"` suffix when `--guess` is active.
      JSON output: `"replacement_char_matches"` array with fields
      `{ byte_offset, line, col, context, suggested, suggested_char }` (nulls
      when no suggestion); MCP JSON output mirrors the same schema.

**Integration tests (theme):** `tests/doctor.rs` — four new scenarios:

1. `replacement_char_residue_detected_and_not_peelable` — file with two
   `U+FFFD` chars is flagged; `mojibake_matches` is empty; `peel_suggested`
   is `None`; `repaired` is false; human output contains `"lossy-replacement"`.
2. `replacement_char_guess_suggests_em_dash_when_space_flanked` — `--guess`
   mode annotates a space-flanked `U+FFFD` with `suggested = Some('—')`;
   human output contains `"U+2014"`.
3. `allow_replacement_char_marker_suppresses_fffd_only` — file with both
   `U+FFFD` and peelable mojibake + the `allow-replacement-char` marker:
   `replacement_char_matches` is empty, `mojibake_matches` is non-empty.
4. `file_with_no_fffd_stays_clean` — clean UTF-8 file with real em-dashes
   and accents is not flagged.

**Status:** ✅ Complete.  All struct literals in external test files
(`tests/doctor.rs`, `tests/end_to_end_corruption_loop.rs`) and
`tpu-mcp/src/tools.rs` updated.  Full workspace: 2 959 tests, 0 failures
(2 868 in `tpu`, 91 in `tpu-mcp`).


---

## Milestone 7 — `replace` zero-match is silently indistinguishable from success

**Theme:** a `tpu replace` / `tpu_replace_in_file` call whose pattern matches
zero times currently reports `status: success` with a fresh `mtime`/`size`
stamp — the same shape a real replacement returns.  The file was rewritten
to identical content (mtime bumped, `.bak` created and then deleted by the
MCP wrapper) and the operator has no unambiguous inline signal that nothing
matched.  Root cause: the reporter's pattern was anchored across a word-wrap
boundary that didn't exist in the file, and the tool made the resulting
zero-match invisible.

**Verification against current code (2026-08-12):**

- `crates/tpu/src/cmd/replace.rs::run` unconditionally calls
  `redwing::materialize` + `crate::atomic_write` even when `splices` is
  empty, so a zero-match run *does* bump the file's `mtime` and does write
  (then rename) a `.bak`.  The MCP wrapper cleans up the `.bak` afterwards
  via `delete_bak_if_exists`, but the mtime bump on the original persists.
- `crates/tpu-mcp/src/tools.rs::call_replace_in_file` emits
  `{ status, file, mtime_epoch_ms, size, changed_lines }` on the normal
  write path.  `changed_lines` *would* be `0` on a zero-match, so the
  situation is technically observable, but there is no explicit
  `count`/`changed` field and `changed_lines` (matched-span lines +
  replacement lines) is a slightly awkward proxy for "did anything match."
  The reporter's ask for an explicit match count is fair.

- [x] M7-1: `tpu::cmd::replace::run` now short-circuits when
      `splices.is_empty() && line_ending_override.is_none() &&
      !count_only && !dry_run`, returning `Ok(0)` before `materialize` /
      `atomic_write` / mojibake guard -- so a zero-match call preserves
      the file's `mtime`, does not write a `.bak`, and avoids one wasted
      full-file rewrite.  The `line_ending_override.is_none()` guard
      preserves the pre-existing `IT-RLE-3` behaviour where
      `--line-ending=lf` still normalises endings on a zero-match run
      (the override is itself a real change to the file).
- [x] M7-2: `cmd::replace::tests::zero_match_preserves_mtime_and_writes_no_bak`
      captures mtime before + after a non-matching-pattern run and
      asserts (a) return value `0`, (b) mtime unchanged, (c) no `.bak`,
      (d) file bytes untouched.  Also updated two pre-existing tests
      that encoded the old contract:
      `fs_replace_default_literal_zero_match_exits_ok` and
      `replace_nomatch_bak_content_equals_original` (renamed to
      `replace_nomatch_leaves_original_untouched_and_writes_no_bak`)
      now assert the new no-op contract; `replace_suite!`'s
      `match_creates_bak` pattern changed from `[a-zA-Z0-9]` to `.` so
      it still fires on `json_no_keys.txt` (which contains only `{}`
      and no alphanumerics).
- [x] M7-3: `tpu-mcp/src/tools.rs::call_replace_in_file`'s success
      status JSON now always includes `"count": n` (the return value
      from `replace::run`).  `"changed_lines"` is kept for back-compat.
      When `n == 0` it also emits
      `"warning": "pattern matched 0 times; file not modified
       (matching is literal by default; pass regex:true for regex)"`
      so a zero-match is visible inline without a follow-up
      `count:true` call, and the warning text pre-empts the
      regex-vs-literal confusion class from the M8-motivating defect
      reports.
- [x] M7-4: `mcp_it_3b_replace_zero_match_reports_count_and_preserves_mtime`
      in `crates/tpu-mcp/tests/mcp_protocol.rs` calls
      `tpu_replace_in_file` with a non-matching pattern and asserts
      the response's status JSON contains `"status":"success"`,
      `"count":0`, `"changed_lines":0`, a `"warning"` field mentioning
      `0 times`, that no `.bak` is created, and that the file's
      `mtime` is unchanged (with a pre-call 50 ms sleep to defeat
      same-timestamp false-passes on fast machines).
- [x] M7-5: Tool description in `tools.rs` updated to note that a
      zero-match run is a no-op (mtime preserved, no `.bak`, response
      includes `count:0` + `warning`) and that the success response
      always includes `count` so no follow-up `count:true` call is
      needed.  Module doc-comment in `crates/tpu/src/cmd/replace.rs`
      gained a "Zero-match short-circuit" section describing both the
      short-circuit and the `line_ending_override.is_none()` guard.

**Status:** ✅ Complete.  Full workspace: 3 055 tests, 0 failures
(793 tpu lib + 785 tpu bin + 1 316 tpu integration + 94 tpu-mcp lib +
17 tpu-mcp mcp_protocol + 44 other suites + 1 ignored doc-test).
`cargo fmt` clean, `cargo clippy --workspace --all-targets` reports
zero new warnings.

**Explicitly out of scope for this milestone** (revisit only if a caller
asks):

- Making zero-match a hard error by default — breaks legitimate
  idempotent-replace workflows (re-running a migration that's already
  been applied).  Could be added later as an opt-in `require_match: true`
  argument if needed.
- Renaming or removing `changed_lines` — back-compat.
- Adding a `changed` boolean — `count` covers it.

**Original defect report:** filed from a defect report where the
reporter's anchor pattern was word-wrapped one word off from the file
and the zero-match success stamp masked the miss.  A second, closely
related defect report ("`tpu_replace_in_file` treats pattern as regex by
default") turned out to describe an older build of the tool -- current
code is literal-by-default and the `reject_removed_fixed_strings_arg`
migration guard in `tools.rs` confirms the flip already landed -- but
the same silent-no-op ambiguity was the core symptom in both reports.
M7's `count` + `warning` fields close that ambiguity for good.


---

## Milestone 8 — Tool/setup version pinning + mismatch detection

**Theme:** make version mismatch between the running `tpu-mcp` binary and
the guidance embedded in `copilot-instructions.md` immediately observable,
so a Copilot session that's been handed a stale extension-bundled binary
(or stale guidance) reports the mismatch on its first `tpu_*` call instead
of silently reproducing bugs that have already been fixed.

**Motivation:** two defect reports in a row (the M7 zero-match report, and
the follow-up "regex-by-default" report) both described behaviour that does
not reproduce against the current codebase.  The most likely explanation
in each case was a stale binary — either the VS Code extension bundled an
older `tpu-mcp.exe`, or the reporter's session started before a recent
rebuild.  Neither reporter noticed, and Copilot had no signal to notice on
its behalf.  A cheap, always-on version echo closes that loop.

**Sources of truth:**

- `crates/tpu-mcp/src/tools.rs::invocation_header` already emits an
  `x-tpu-mcp-invocation` JSON object as the first NDJSON line of every
  tool response — the natural place to hang `"tpu_version"`.
- `crates/tpu/src/cmd/setup.rs::guidance_body` emits the canonical
  guidance block injected between `<!-- tpu-mcp:setup:begin -->` /
  `<!-- tpu-mcp:setup:end -->` markers — the natural place to pin the
  version the guidance was written for.
- `env!("CARGO_PKG_VERSION")` is already used in
  `crates/tpu-mcp/src/main.rs` for the startup banner.

- [x] M8-1: `tpu-mcp/src/tools.rs::invocation_header` now emits
      `"tpu_version": env!("CARGO_PKG_VERSION")` (the `tpu-mcp` binary's
      own version) as an extra field on the invocation-header JSON.
      One extra field per response, no per-tool schema change; the
      function's doc-comment now describes the new field and its
      intended use.
- [x] M8-2: `tpu::cmd::setup::guidance_body` now uses `concat!(...)`
      to prepend `<!-- tpu-mcp:setup:version=<CARGO_PKG_VERSION> -->

`
      as the first line of the injected body, so re-running
      `tpu setup --inject` always refreshes the marker to the running
      `tpu` binary's version.  Full-block round-trip preserves the
      HTML-comment form so it stays invisible in rendered Markdown.
- [x] M8-3: A new `### Version check (do this first)` subsection sits
      immediately after the intro paragraph in the injected guidance
      body.  It directs Copilot to compare the `tpu_version` field on
      the first `x-tpu-mcp-invocation` line against the
      `tpu-mcp:setup:version=` marker at the top of the block and, on
      mismatch, to stop and report both versions plus the appropriate
      remedy (reinstall the extension for binary-older-than-guidance;
      re-run `tpu setup --inject` for binary-newer-than-guidance)
      before performing any file mutation.
- [x] M8-4: `mcp_it_1b_invocation_header_includes_tpu_version` in
      `crates/tpu-mcp/tests/mcp_protocol.rs` calls `tpu_read_file` and
      asserts the first non-empty line of the response is a JSON object
      with `reason=="x-tpu-mcp-invocation"` and `tpu_version` equal to
      `env!("CARGO_PKG_VERSION")` of `tpu-mcp` at test compile time.
- [x] M8-5: `setup_emits_version_marker_matching_cargo_pkg_version` in
      `crates/tpu/tests/copy_render_setup.rs` asserts the plain-print
      output, a fresh `--inject`, and a re-inject over a stale block
      each contain `<!-- tpu-mcp:setup:version={env!("CARGO_PKG_VERSION")} -->`,
      and additionally that the re-inject removes the stale
      `0.0.0-stale` marker while preserving trailing user content.
- [x] M8-6: `crates/tpu-mcp/README.md` gained a `### Version-check
      directive` subsection under "Protocol", and its tool-output-format
      table now shows the `tpu_version` field on the invocation header.
      `crates/tpu-mcp/extension/README.md` gained a `## Bundled binary
      version + version-check directive` section tying the pinned
      bundled `tpu-mcp.exe` to the "Show bundled server version"
      command and giving the one-command remedy for each drift
      direction.

**Explicitly out of scope for this milestone** (revisit only if a caller
asks):

- Enforcing the check server-side (refusing to serve tool calls when
  versions differ).  Too aggressive — Copilot may legitimately be
  operating against a checkout where the guidance is intentionally ahead
  or behind of the currently-running binary during upgrade dance.
  Reporting-and-let-the-model-decide is enough.
- Cross-version semver-tolerance logic.  Exact-match reporting is simpler
  and catches the real defect (stale bundled binary) without needing a
  compat matrix.  A future minor version bump can add a `>=` tolerance
  if this proves too noisy.
- Reconciling the `tpu` vs `tpu-mcp` crate versions.  They may drift
  independently; the guidance marker records the version of whichever
  crate ran `tpu setup`, and the invocation header records the version
  of the `tpu-mcp` binary that answered the call.  Mismatch reporting
  works correctly either way.

**Status:** ✅ Complete.  Full workspace: 3 057 tests, 0 failures
(793 tpu lib + 785 tpu bin + 20 copy_render_setup + 1 316 tpu integration +
94 tpu-mcp lib + 18 tpu-mcp mcp_protocol + 44 other suites + 1 ignored
doc-test).  `cargo fmt` clean, `cargo clippy --workspace --all-targets`
introduces zero new lints (the 41 flagged fixes are all pre-existing
style-only lints in files this milestone did not modify).  Filed after
two defect reports whose evidence pointed at an older binary being used
against present-day expectations, without any inline signal that would
have let Copilot notice.

---

## Milestone 9 — zero-match `replace`: mtime bump via the verification stamp, and error-by-default

**Theme:** M7-1 stopped a zero-match `tpu_replace_in_file` from *rewriting*
the file, but the MCP wrapper then ran the write-verification stamp
unconditionally.  `stamp_and_verify` opens the file for write and sets its
mtime to "now", so the supposed no-op still advanced mtime — the exact
symptom M7 set out to eliminate, just moved one layer up.

**Defect report (2026-08-27):** a zero-match call left the content hash
identical (`sha256` unchanged, no `.bak`) but advanced mtime by 20 s, and
the reported `mtime_epoch_ms` matched the new on-disk value exactly,
proving a real metadata write rather than a stale clock read.  This makes
`tpu_stat_file` — documented as the way to "verify a write actually
persisted" — unsound as a change detector, and defeats mtime-based
watchers and incremental builds.

**Why M7-4 did not catch it:** every `McpSession` in
`crates/tpu-mcp/tests/mcp_protocol.rs` spawns the server with
`--verify-delay-ms=0`, and the `call()` helper in `tools.rs`'s
`integration_tests` hardcoded the same.  `delay_ms == 0` takes an early
return that only *reads* metadata, so the entire stamping path — the code
that actually mutates mtime — was never exercised by any test.  Production
defaults to `verify_delay_ms: 100`.

**The other half of the report** (`status: "success"` with no `count` and
no `changed`) did **not** reproduce: current `call_replace_in_file` always
emits `"count": n` and, when `n == 0`, a `warning`.  The reporting session
was running a pre-M7 binary — precisely the drift M8's `tpu_version` echo
exists to surface.

- [x] M9-1: `call_replace_in_file` now computes `wrote = n > 0 ||
      le_override.is_some()` (the same condition already gating
      `delete_bak_if_exists`) and calls `stamp_and_verify` only when
      `wrote`.  Otherwise it calls the new `read_stamp`, so the response
      still reports accurate `mtime_epoch_ms`/`size` without mutating
      them.  There is nothing to verify when nothing was written, and
      stamping would otherwise be the call's only mutation.
- [x] M9-2: New `read_stamp(file)` helper reads mtime/size without
      modifying the file; `stamp_and_verify`'s `delay_ms == 0` early
      return now delegates to it instead of duplicating the metadata
      read.  `stamp_and_verify`'s doc-comment states that it must only be
      called when a write actually occurred.
- [x] M9-3: `call()` in `tools.rs`'s `integration_tests` now delegates to
      a new `call_with_verify_delay(name, args, verify_delay_ms)` so a
      test can opt into the production stamping path.  Existing callers
      keep the zero-delay behaviour unchanged.
- [x] M9-4: `replace_zero_match_preserves_mtime_with_verification_enabled`
      runs a non-matching replace with `verify_delay_ms: 100` and asserts
      `count:0`, an inline `warning`, unchanged mtime, no `.bak`,
      untouched bytes, and that the reported `mtime_epoch_ms` equals the
      untouched on-disk mtime.  Verified to fail against the pre-fix code
      and pass after.  Companion
      `replace_with_match_still_stamps_and_verifies` pins the fix to
      "skip stamping only when nothing was written" by asserting a real
      match still advances mtime and reports `count:1` with no warning.

- [x] M9-5: **Breaking change** — a zero-match `tpu_replace_in_file` is now
      an ERROR by default rather than a success.  M7 deferred this ("breaks
      idempotent re-runs") and offered a future opt-in `require_match: true`;
      the same defect recurred a third time, so the default is flipped and
      the escape hatch inverted: `allow_no_match: true` restores the old
      success-with-`count:0`-and-`warning` shape for genuinely idempotent
      workflows.  Rationale: the failure mode is asymmetric.  A wrongly-
      errored idempotent re-run is loud and trivially fixed by one argument;
      a wrongly-succeeded mis-anchored pattern is silent and was caught only
      because `git status` happened to be clean.
      - The check runs *before* `delete_bak_if_exists` and the stamp, so an
        erroring no-op leaves the filesystem completely untouched.
      - `count:true` and `dry_run:true` are exempt — they are introspection
        modes where a zero result is the legitimate answer, never an error.
      - A `line_ending` override is also exempt: it rewrites the file to the
        requested convention even with zero substitutions, so the call did
        real work and still reports success (with the `warning`).
- [x] M9-6: Tests for the new contract:
      `replace_zero_match_preserves_mtime_with_verification_enabled` (now
      asserts the default error path leaves mtime, `.bak`, and bytes
      untouched), `replace_zero_match_with_allow_no_match_succeeds_and_
      preserves_mtime`, and `replace_zero_match_count_and_dry_run_never_
      error`.  `mcp_it_3b` and `mcp_it_3c` in `mcp_protocol.rs` were updated
      to the new contract; 3c now checks that *both* zero-match outcomes
      preserve a pre-existing `.bak`.
- [x] M9-7: Tool description and the new `allow_no_match` schema property
      document the error-by-default contract, the opt-out, and the
      count/dry_run exemption.

**Status:** ✅ Complete.  `cargo test -p tpu-mcp`: 137 passed, 0 failed
(102 bin incl. the 4 new tests + 6 io_worker_chaos + 29 mcp_protocol);
`cargo test -p tpu`: 2 966 passed, 0 failed, 1 ignored.
`cargo fmt --check` clean; `cargo clippy -p tpu-mcp --all-targets`
introduces no new lints.  Additionally verified end-to-end against a real
`tpu-mcp.exe` at the default `verify_delay_ms: 100`: a zero-match call
preserved mtime to the tick and wrote no `.bak`, while a matching call
still wrote, stamped, and reported `count:1`.

---

## Milestone 10 — `tpu_replace_in_file` replacement is verbatim; escape expansion is opt-in

**Theme:** the MCP `replacement` argument was the only text payload in the
server that got a second decoding pass.  `decode_replacement_arg` ran
`unescape_replacement()` on every plain-JSON replacement, so `\\` collapsed
to `\`, `\n` became LF, and `\t` became TAB — while `tpu_write_file`'s
`content`, `tpu_append_file`'s `text`, and an edit op's `data` all wrote
their bytes verbatim.  A caller had no way to know which arguments needed
pre-doubling and which did not.

**Defect report (2026-08-28):** a controlled probe showed `\\` landing as
`\` and `\\\\` landing as `\\` through `tpu_replace_in_file`, while the same
content through `tpu_append_file` was correct.  The collapse silently broke a
live code path — a replacement containing `path.starts_with(r"\\.")` was
written as `r"\."`, disabling the very guard the caller was adding — and
mangled several doc strings before it was noticed.  The reporter also found
the same collapse in prose predating the session, so the damage had been
accumulating quietly.

**Why the old default was wrong:** its stated motive was that an agent
sending `\n` "expects a newline".  But JSON transport already delivers a real
newline for a correctly escaped `"\n"`, so the pass only ever fired on
*doubled* escapes — and it collapsed them unconditionally, corrupting every
replacement whose payload legitimately contains backslashes (Rust/C/JSON
string literals, regex sources, Windows/UNC paths).  The failure mode is
asymmetric in the same way M9's was: an unexpanded `\n` is visible in the
changed-region echo and trivially fixed by one argument, whereas a collapsed
`\\` reads as plausible code and survives review.

- [x] M10-1: `decode_replacement_arg` writes the plain-JSON `replacement`
      verbatim (LF-normalised, like every other text payload) instead of
      calling `unescape_replacement`.
- [x] M10-2: New `expand_escapes: true` argument restores the old sed-style
      decoding for callers that deliberately double-escape.  Combining it
      with `replacement_format` is rejected — that channel already carries
      exact bytes, so re-decoding them is contradictory.
- [x] M10-3: New `optional_bool` helper: a present-but-non-boolean
      `expand_escapes` is an error rather than a silent `false`, so a caller
      who sends `"true"` learns the flag did not take effect.
- [x] M10-4: Tool description and schema document the verbatim contract, the
      opt-in, and the mutual exclusion.  The ESCAPE-HAZARD paragraph now
      attributes the residual risk to JSON transport alone.
- [x] M10-5: Tests — RE-IT-1…4 now pass `expand_escapes: true` (they pin the
      opt-in path); RE-IT-5 pins the reported `r"\\."` regression, RE-IT-6
      pins source-level `\t`/`\n` sequences surviving, RE-IT-7 pins that a
      real newline still works, RE-IT-8/9 pin the two rejection paths.
      `mcp_it_4` in `mcp_protocol.rs` covers both defaults end-to-end over
      real stdio.

**Deliberately NOT changed:** the `tpu replace` CLI keeps escape decoding by
default (`--literal-replacement` / `-L` to opt out).  A shell user typing
`\n` at a prompt is a different transport with `sed`/`perl`/`ripgrep`
conventions; the reported defect is specific to JSON-RPC arguments, where the
transport has already resolved escapes before tpu sees them.

**Status:** ✅ Complete.  `cargo test -p tpu-mcp`: 145 passed, 0 failed
(110 lib incl. the 8 new tests + 6 io_worker_chaos + 29 mcp_protocol);
`cargo test -p tpu` unaffected.  `cargo fmt --check` clean; `cargo clippy
-p tpu-mcp --all-targets` introduces no new lints.

### Review follow-up — the same silent-ignore class in the `*_format` accessors

Code review of the fix found that M10-2's conflict guard could be bypassed,
by the *same* class of defect the milestone exists to remove.
`decode_replacement_arg` dispatched on
`args.get("replacement_format").and_then(|v| v.as_str())`, which collapses a
present-but-non-string value (`123`, `true`, `["base64"]`) to `None`.  A call
carrying `replacement_format: 123` therefore fell silently into the plain-text
branch: the conflict check never fired, the caller's encoded payload was
written as its literal encoded text, and with `expand_escapes: true` also set
the escape decoding ran anyway — reintroducing the exact second decoding pass
this milestone removes.  Reproduced end-to-end over stdio before fixing.

- [x] M10-6: New `optional_format_str` helper errors on a present-but-non-
      string `{key}_format`, mirroring `optional_bool`.  Applied to all three
      accessors — `decode_replacement_arg`, `decode_content_arg`, and
      `decode_pattern_arg` — so `content_format` / `pattern_format` cannot be
      silently dropped either.  The conflict check now keys on the argument's
      presence rather than on successful string extraction.
- [x] M10-7: `tpu_edit_file`'s per-op `data_format` had the same shape via
      `find_map(|op| op.get("data_format").and_then(|v| v.as_str()))`, which
      *skipped* a non-string value and moved on to later ops.  Now a
      non-string `data_format` on any op is an error; the "first op that
      specifies one wins" semantics is preserved.  This matters because the
      guidance written by M10-4 points callers at `*_format` as the reliable
      escape-hazard-free channel — a channel that can be silently ignored
      would make that advice unsound.
- [x] M10-8: Tests — RE-IT-10 (non-string `replacement_format`, the reported
      bypass), RE-IT-11 (verbatim survives `regex:true` + capture-group
      `expand()`, a gap review found untested), RE-IT-12 (non-string per-op
      `data_format`).  RE-IT-8/9 were strengthened via a new
      `assert_rejection_was_inert` helper that asserts bytes, mtime, AND
      absence of `.bak` — previously they asserted content only, leaving the
      Milestone 7/9 regression class unpinned on the new rejection paths.
- [x] M10-9: The agent-facing version note in `.github/copilot-instructions.md`
      keyed the hazard to "up to and including 2.0.0", but the crate is still
      at 2.0.0 on this branch, so the *fixed* server also reports 2.0.0 and the
      check could never distinguish fixed from unfixed.  Reworded to "released
      builds before 3.0.0", with an explicit note that a dev build reports the
      pre-release version and a one-shot behavioural probe as the decisive
      check.
