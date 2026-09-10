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

---

## Milestone 11 — Mutation-testing coverage: Tier 2 (protocol & core-loop hot paths)

**Theme:** a `cargo mutants` run against the full workspace (2026-09-08) found
444 unique missed mutants and 36 unique timeouts (raw totals were ~1.6x
inflated by two appended runs into one `mutants.out`; always dedupe before
trusting a count). Files were bucketed into three tiers by missed-mutant
count. Tier 1 (`git.rs`, `cmd/doctor.rs`, `mojibake.rs`, `cmd/copy.rs` — the
highest-impact files) is complete; `tpu-mcp/src/tools.rs` (also Tier 1) is
partially done — its pure helper functions are covered, but the `&&`/`||`/`!`
argument-combination mutants inside the MCP `call_*` endpoint handlers
(`call_write_file`, `call_replace_in_file` x6, `call_edit_file` x4,
`call_find` x5, `call_render_file` x4, `call_count_file` x3,
`call_copy_file`'s `Write` impl, `render_changed_regions`, `stamp_and_verify`)
remain and are tracked as a carry-over, not part of this milestone.

This milestone covers Tier 2: the next six highest-count files (125 missed
mutants combined), which cluster around two themes — manual index-stepping
loops (the same category responsible for most of the 36 timeouts) and the
MCP stdio worker protocol (`tpu-mcp/src/main.rs` + `worker.rs`), which are
best fixed together since they share the same request/response shape.

**Cross-cutting groundwork (do first, applies to every file below):**

- [x] M11-1: Re-run `cargo mutants` into a fresh `--output` directory (not an
      appended `mutants.out`) before starting and after this milestone lands,
      so the before/after score is trustworthy. **Complete.**
      **Before:** `cargo mutants -f crates/tpu-mcp/src/worker.rs -f
      crates/tpu-mcp/src/main.rs -f crates/tpu/src/lib.rs -f
      crates/tpu/src/encoding.rs -f crates/tpu/src/cmd/render.rs -f
      crates/tpu/src/data_format.rs -o mutants.out.tier2-before --no-shuffle
      -j 6` (bare `cargo test`, started *before* any M11-2/M11-3 changes)
      finished in 2h: **849 mutants tested: 185 missed, 596 caught, 32
      unviable, 36 timeouts**. This run's copy of the source predates the
      M11-2 fixes (confirmed: its `timeout.txt` still lists `retry_io`'s
      three now-fixed mutants at lib.rs:390-391), which empirically confirms
      `cargo mutants` copies the source tree once at invocation start — live
      edits made to the working tree *after* a run starts do not leak into
      that run's results, so it was safe to keep editing
      `lib.rs`/`worker.rs`/`main.rs` for M11-2 while this "before" run
      executed in the background.
      **After:** the same six-file scope, now via `.cargo/mutants.toml`'s
      default `test_tool = "nextest"` (M11-3), run once every per-file item
      (M11-5 through M11-10) had landed: **716 mutants tested: 16 missed, 656
      caught, 38 unviable, 0 timeouts.** (Mutant counts aren't directly
      comparable 1:1 across runs — refactors that removed loop-stepping
      variables, e.g. M11-7's/M11-8's iterator rewrites, also remove the
      mutation sites those variables offered — but the shape of the change
      is unambiguous: missed mutants dropped by 91% (185 → 16) and every one
      of the 36 pre-existing timeouts is gone (36 → 0), the direct payoff of
      M11-3's nextest adoption.) Every one of the 16 remaining missed
      mutants is individually documented in M11-5 through M11-10 as either
      mathematically/behaviourally equivalent or a platform-coverage gap —
      none are unaccounted for:
      | file | before (missed/timeout) | after (missed/timeout) |
      |---|---|---|
      | `crates/tpu/src/encoding.rs` | 48 / 8 | 0 / 0 |
      | `crates/tpu/src/cmd/render.rs` | 40 / 8 | 1 / 0 |
      | `crates/tpu/src/data_format.rs` | 37 / 4 | 7 / 0 |
      | `crates/tpu-mcp/src/main.rs` | 29 / 2 | 1 / 0 |
      | `crates/tpu-mcp/src/worker.rs` | 16 / 4 | 2 / 0 |
      | `crates/tpu/src/lib.rs` | 15 / 10 | 6 / 0 |
      Both outputs preserved at `mutants.out.tier2-before/` and
      `mutants.out.tier2-after/` (gitignored, ~100-150MB each) for anyone
      who wants to inspect the raw reports directly.
- [x] M11-2: Decide the `mutants::skip` policy for sites that will hang under
      *any* test correctly exercising their real failure path, because the
      loop bound or response write is removed with no independent cap.
      **Decision, per site** (the milestone's original wording undersold two
      of these as simple `mutants::skip` candidates and mischaracterized
      `IoWorker::call`'s id-matching as "fast-but-uncovered" — both were
      re-verified against the raw `timeout_unique.txt` report before fixing):
      - `retry_io`'s `attempt < MAX_RETRIES && is_transient_io_error(&e)`
        guard (mutated to `true`/`&&`→`||`/`attempt+=1`→`*=1`) is a **genuine
        infinite loop** given a persistently-failing closure, but `retry_io`
        is a pure function with no subprocess involved — fixed by wrapping
        the three affected `retry_io_tests` in a bounded-timeout background
        thread (`run_with_timeout`, `mpsc::channel` + `recv_timeout`) so each
        now fails in ~2s instead of hanging 112s+. No skip needed.
      - `worker.rs`'s `write_response` and `main.rs`'s `send_response`/
        `send_error` mutated to `()` cause the parent's `read_line` to block
        forever on a real subprocess round trip, but all three are already
        generic over `impl io::Write` — fixed by adding direct unit tests
        against an in-memory `Vec<u8>` buffer (no subprocess, no possibility
        of hanging). No skip needed.
      - `main.rs`'s notification-vs-request `v.is_null()` check was likewise
        fixable directly: extracted into a pure `request_id(Option<Value>)
        -> Option<Value>` helper with direct unit tests (`None`, explicit
        `null`, a non-null falsy `0`, and ordinary values). No skip needed.
      - `IoWorker::call`'s response-id matching (`worker.rs`) is a **genuine
        cumulative-slowdown timeout**, not a fast bug: any mutation to its
        `n==0`/`resp_id!=id` checks misclassifies every real round trip as
        "worker dead", triggering a bounded (~1.7-2s) respawn+backoff on
        *every* call; the existing `tests/io_worker_chaos.rs` chaos suite
        makes dozens of real round trips per test, so the cumulative cost
        exceeds cargo-mutants' ~112s auto-timeout even though no single call
        hangs forever. Faking this at the unit level would require mocking
        concrete `ChildStdin`/`ChildStdout` types. **Accepted** via
        `#[cfg_attr(test, mutants::skip)]` on the whole method — verified
        zero already-caught mutants are sacrificed by this skip. Required
        adding `mutants = "0.0.4"` as a **regular** (not dev-) dependency of
        `tpu-mcp`, since `cfg_attr(test, mutants::skip)` must still parse in
        all profiles cargo-mutants builds; cargo-mutants does not evaluate
        the `test` cfg condition itself — the mere syntactic presence of the
        attribute is what triggers the skip.
      - `IoWorkerHandle::try_call`'s `!have_worker` site was left undecorated
        at the time this decision was made (function-level skip would have
        sacrificed 3 other already-caught mutants; `mutants::exclude_re`
        isn't published yet — max published version 0.0.4, needs 0.0.5+).
        **Update after M11-3 landed:** this site is now caught anyway,
        without any further code change. Once M11-3 adopted nextest as the
        test tool, the same cumulative-slowdown that made
        `IoWorker::call`'s mutants time out now gets forcibly terminated by
        nextest's per-test `slow-timeout` (60s cap) and reported as a normal
        test failure — verified in the M11-5/6/7 comprehensive re-run (see
        M11-5). No further action needed here.
- [x] M11-3: Evaluate installing `cargo-nextest` with a configured
      slow-timeout / terminate-after profile, both so real-world hangs fail
      fast in ordinary `cargo test`/CI runs and so `cargo mutants` can be
      pointed at nextest as its test runner instead of bare `cargo test`.
      **Adopted.** nextest runs each test in its own OS process, so it can
      forcibly terminate a single hung test (via Windows job objects / a
      Unix process-group SIGTERM-then-SIGKILL) instead of the whole test
      binary blocking forever the way bare `cargo test` does. Added
      `.config/nextest.toml` with `[profile.default]` (`fail-fast = false`,
      matching `ci.yml`'s prior `--no-fail-fast` policy; `slow-timeout =
      { period = "20s", terminate-after = 3 }`, i.e. a 60s forced-termination
      cap) and `[profile.mutants]` (inherits the same `slow-timeout`, restores
      nextest's own fail-fast-on-first-failure default). Thresholds were
      chosen from real data: `cargo nextest run` with no config measured the
      slowest legitimate test in the workspace at ~8.2s
      (`tpu-mcp::io_worker_chaos::chaos_kill_between_writes_all_succeed`,
      real subprocess kill/respawn cycles), so 60s gives ~7x margin before a
      real test is ever mistakenly killed, while remaining well below
      `cargo-mutants`' own auto-computed per-mutant timeout (observed
      ~112-119s on bare `cargo test`) — so nextest's own termination, not
      cargo-mutants' fallback, is what ends a hung mutant test.
      **Validated empirically**, not just configured: a temporary test
      containing a genuine `loop { sleep(100ms) }` was run under
      `--profile mutants` with a deliberately tightened `slow-timeout =
      { period = "2s", terminate-after = 1 }` and confirmed to be forcibly
      terminated and reported as a failing (`TIMEOUT`) test at ~2.1s instead
      of hanging — then both the probe test and the tightened config were
      reverted. A real `cargo mutants -f crates/tpu/src/cmd/count.rs
      --test-tool=nextest` end-to-end run (42 mutants, a file untouched by
      Tier 2) also completed cleanly: 42 caught, 0 missed, 0 timeouts.
      Added `.cargo/mutants.toml` with `test_tool = "nextest"` and
      `additional_cargo_test_args = ["--profile=mutants"]` so this is now the
      default for every future `cargo mutants` invocation in this workspace
      (M11-1's "after" run, all of Milestone 12, and beyond) without needing
      the flag repeated on the command line. `ci.yml`'s `build-test` and
      `build-test-windows` jobs now install nextest
      (`taiki-e/install-action@nextest`) and run `cargo nextest run
      --workspace --locked` instead of bare `cargo test --no-fail-fast`,
      followed by a `cargo test --doc --workspace --locked` step, since
      **nextest does not run doctests** (a known, deliberate nextest
      limitation — see nextest-rs/nextest#16). This workspace currently has
      zero runnable doctests (only ` ```text` /` ```rust,ignore` ` fences,
      confirmed by grep before making this change), so `cargo mutants
      --test-tool=nextest` loses no coverage today; re-check this if a real
      ` ```rust` ` doctest is ever added, since cargo-mutants does not run
      doctests itself even when using the default `cargo test` tool.
      Full workspace `cargo nextest run`: 3 688 tests passed, 0 skipped, in
      ~50s; `cargo fmt --check` clean.
- [ ] M11-4: Where a manual `while i < len { …; i += 1 }` loop can be
      refactored to an iterator-based loop without changing behaviour, prefer
      that refactor over adding a test — it removes the mutation site (and
      the timeout) entirely rather than merely covering it. Only fall back to
      a direct fast unit test (or a documented skip) where the raw index is
      load-bearing. Applied three times so far: `TextLayout::analyze` in
      M11-7 (byte stream, `bytes.iter().enumerate()`), and
      `replace_u16_pairs`/`normalize_bytes_to_lf`/`normalize_u16_to_lf` in
      M11-8 (`chunks_exact(2)` for the 2-byte-unit functions, `enumerate()`
      for the byte-oriented one) — all four eliminated their `+=` timeout
      mutants outright rather than merely covering them, and M11-8's file
      went from 24 missed/4 timeouts to a perfect 0 missed/0 timeouts. The
      same manual-index-loop pattern still exists, unaddressed, in
      cmd/render.rs (M11-9).

**Per-file work items:**

- [x] M11-5: `crates/tpu-mcp/src/worker.rs` (16 missed in the M11-1
      baseline). **Complete.** `write_response` closed by 4 direct
      in-memory-buffer unit tests. `IoWorker::call`'s response-id matching
      accepted via `#[cfg_attr(test, mutants::skip)]` (M11-2). The retry
      loop's `+`/`-`/`+=`/`>=` arithmetic (max-attempts computation, backoff
      indexing, the retry/give-up boundary) was extracted into two pure,
      directly-tested functions (`max_attempts`, `decide_retry` returning a
      `RetryDecision` enum) shared by both retry sites, plus
      `should_log_retry_success`; this is the same "extract testable pure
      logic" refactor M11-4 recommends for loops, applied here to
      subprocess-adjacent retry bookkeeping instead. `IoWorkerHandle::
      try_call`'s `!have_worker` site — previously left undecorated pending
      `mutants::exclude_re` — is now caught automatically by M11-3's nextest
      adoption (see M11-2's update note) with no further code change.
      `WorkerCallError::is_worker_dead` mutated to `true` is accepted as
      equivalent: with only two variants (`PipeBroken`, `Protocol`) and both
      matched as dead, the function is behaviourally always-`true` today by
      construction; pinned with a direct test of the *intended* semantics
      rather than chasing the unkillable mutant. `IoWorker::drop` mutated to
      `()` is accepted as not practically testable: dropping `IoWorker` also
      drops its `stdin: ChildStdin` field right after, which alone makes a
      healthy worker exit via EOF on its own read loop, so the explicit
      `kill()`/`wait()` is only observably necessary for an *unresponsive*
      worker — confirmed by hand-mutating `drop()` to `()` and re-running
      `drop_terminates_the_child_process` (still passes). Distinguishing
      this would need either a flaky race (checking immediately after
      `drop()` returns, since only the real `wait()` makes that instant
      deterministic) or new test-only instrumentation to make a worker
      ignore stdin EOF on purpose; not pursued. Verified via a real scoped
      `cargo mutants` run (see M11-7's verification note): 2 missed (both
      documented above), 0 timeouts.
- [x] M11-6: `crates/tpu-mcp/src/main.rs` (29 missed in the M11-1 baseline).
      **Complete.** `v.is_null()` extracted into a pure `request_id` helper;
      `send_response`/`send_error` closed via direct in-memory-buffer tests
      (M11-2). `code::PARSE_ERROR`/`METHOD_NOT_FOUND`/`INVALID_PARAMS`
      (negative-literal deletion) pinned with an exact-value test.
      `dispatch`'s `"ping"`/`"shutdown"`/unknown-method match arms covered
      with direct calls asserting the exact `ResponseBody`. `log_info`/
      `log_warn` mutated to `()` closed the same way as `send_response`.
      `parse_config`'s CLI-argument loop was extracted into a pure
      `apply_arg(&mut ArgFlags, &str)` function (mirroring M11-5's retry-loop
      extraction), directly tested for every flag (`--verify-delay-ms=`,
      `--default-on-error=`, `--progress-detail=`, `--quiet`,
      `--eol-normalize`, the worker sentinel args, and an unrecognised arg).
      `parse_config`'s env-var-driven defaults (`TPU_MCP_QUIET`,
      `TPU_MCP_NO_IO_WORKER`, `TPU_EOL_NORMALIZE`, `TPU_DEFAULT_ERROR_MODE`,
      `TPU_PROGRESS_DETAIL`) are covered by tests that mutate real process
      env vars directly, serialised behind a `Mutex` (`ENV_VAR_TEST_LOCK`) so
      they are correct under *both* nextest's per-test-process isolation
      *and* plain `cargo test`'s default multithreaded-single-process model
      (confirmed by hand-testing under both runners — the unguarded version
      raced and failed intermittently under plain `cargo test`). One mutant
      accepted as equivalent: `apply_arg`'s `s == worker::WORKER_ARG` branch
      body is empty (a documented no-op), so mutating `==` to `!=` produces
      identical behaviour for every possible input. Verified via a real
      scoped `cargo mutants` run: 1 missed (documented above), 0 timeouts.
- [x] M11-7: `crates/tpu/src/lib.rs` (23 missed in the original noisy
      report; 15 missed + 10 timeouts in the clean M11-1 baseline).
      **Complete.** The `retry_io` boundary mutants and its three timeout
      mutants were already closed by M11-2. `TextLayout::analyze`'s manual
      byte-stepping loop was refactored to an iterator-driven walk (per
      M11-4) — the loop no longer has a manual index to mutate into an
      infinite loop, only a lookahead `bytes.get(idx + 1)` expression, which
      is directly tested (8 new tests covering every terminator kind, the
      CRLF-guard boundary, and the trailing-line arithmetic). `WriteLock`/
      lock-file logic: `lock_sidecar_path`'s exact 255-char boundary is
      pinned by a boundary test; `open_lock_file`'s access/share-mode flags
      are pinned by a new test that actually reads and writes through the
      returned handle (rather than relying on a side effect like
      delete-on-close, which this Windows version turns out to honour even
      with a bit missing from the mask) — this caught the one *not*
      equivalent bitflag mutant (`|`→`&` on the second `access_mode`
      operator, which collapses to `GENERIC_READ` alone because `&` binds
      tighter than `|` in Rust, canceling `GENERIC_WRITE & DELETE` to `0`);
      `acquire_write_lock`'s contention-wait boundary (`>=` vs `<` on
      `LOCK_WAIT_CAP`) is pinned by a real two-thread contention test.
      Four mutants accepted as equivalent: the remaining `|`→`^` bitflag
      swaps (both `access_mode` and `share_mode`, both operator positions)
      are mathematically identical to `|` because every operand is a
      disjoint single-bit (or already-disjoint) constant — confirmed by
      hand-mutating each and re-running the full `write_lock_tests` module
      (all pass regardless). `WriteLock::drop` mutated to `()` is accepted
      as equivalent for the same reason as `IoWorker::drop` above: the
      `file: fs::File` field's own drop closes the handle (releasing the OS
      lock) immediately after the custom `drop()` body returns, whether or
      not that body does anything — confirmed by hand-mutating and
      re-running the full `write_lock_tests` module. One mutant is a
      platform-coverage gap, not a missing test: the `#[cfg(not(windows))]`
      variant of `open_lock_file`'s whole-function-replace mutant is inside
      code that is not compiled at all when `cargo mutants` runs on this
      Windows machine, so no test on this platform can ever observe a
      difference; would need a Linux/macOS `cargo mutants` run to validate.
      **Verification methodology:** every fix in M11-5/6/7 above was
      confirmed by a real scoped `cargo mutants -f crates/tpu/src/lib.rs -f
      crates/tpu-mcp/src/worker.rs -f crates/tpu-mcp/src/main.rs` run (using
      the nextest test tool per M11-3) against the fully-edited source,
      not just by hand-mutating individual sites: **138 mutants tested, 115
      caught, 9 missed (all 9 documented above as equivalent or a platform
      gap), 14 unviable, 0 timeouts** (down from the M11-1 baseline's 849
      mutants across all six Tier 2 files scoring 185 missed / 36 timeouts
      — not a directly comparable subset, but illustrative of the drop from
      dozens of missed/timeout mutants in these three files to a
      single-digit, fully-accounted-for residual).
- [x] M11-8: `crates/tpu/src/encoding.rs` (24 missed / 4 timeouts in the
      original noisy report; 24 missed / 4 timeouts unique, deduplicated in
      the M11-1 baseline). **Complete — perfect score.** All four functions
      (`replace_u16_pairs`, `normalize_bytes_to_lf`, `normalize_u16_to_lf`,
      `apply_line_ending_to_all`) had zero direct unit tests before this
      item. `replace_u16_pairs` refactored from a manual `while i+1 <
      len() { …; i += 2 }` loop to `bytes.chunks_exact(2)` +
      `.remainder()` (per M11-4) — eliminates its `i += 2` timeout mutant
      structurally. `normalize_bytes_to_lf` refactored to the same
      iterator-driven-walk pattern used for `TextLayout::analyze` in M11-7
      (`bytes.iter().enumerate()`, one extra `.next()` to consume a paired
      LF) — eliminates its `i += 1`/`i += 2` timeout mutant and collapses
      the CRLF lookahead from a 3-part bounds-checked condition to one
      `bytes.get(idx + 1) == Some(&0x0A)` comparison. `normalize_u16_to_lf`
      refactored the same way but with `chunks_exact(2)` for 2-byte code
      units, using `chunks.clone().next()` to peek the following unit
      without consuming it — eliminates its two `+=` timeout mutants
      (main-loop stepping and the CRLF-unit-pair stepping) and collapses
      the CRLF-unit lookahead from a 3-condition bounds-checked `&&` chain
      to a single 2-condition comparison. Added 6 tests for
      `normalize_bytes_to_lf` (empty, lone LF, CRLF pair, lone CR not
      followed by LF, trailing lone CR, mixed terminators) and 7 for
      `normalize_u16_to_lf` (the same cases plus an odd-trailing-byte case,
      since UTF-16 has an extra malformed-input path single-byte streams
      don't). `apply_line_ending_to_all`'s `b == 0x0A` → `0x0D` remap
      (target = `Cr`, non-UTF-16 path) pinned by one direct test. **Real
      scoped `cargo mutants -f crates/tpu/src/encoding.rs` run (nextest
      tool): 122 mutants tested, 112 caught, 0 missed, 10 unviable, 0
      timeouts** — every viable mutant in the file is now caught.
- [x] M11-9: `crates/tpu/src/cmd/render.rs` (20 missed / 4 timeouts unique
      in the M11-1 baseline). **Complete.** Unlike M11-7/M11-8, `render_str`
      and `find_close`'s loop indices turned out to be genuinely load-bearing
      per M11-4's own exception clause: `render_str`'s single loop advances
      by three *different* amounts depending on what it finds (an escape
      sequence, a resolved token span, or one UTF-8 scalar's byte width) and
      slices the original `template` string by exact byte offset for the
      `MissingPolicy::Leave` case, so an iterator-based rewrite would be a
      much larger, riskier change for uncertain benefit — especially now
      that M11-3's nextest adoption already provides a safety net (a
      mutation that did create a genuine infinite loop would be terminated
      and reported as a failure, not silently missed or an unbounded hang).
      Went with comprehensive direct tests instead (14 new tests covering
      escape-sequence and token-start boundary conditions, multi-byte UTF-8
      literal text, repeated/counted substitution semantics, and two
      compare-byte-to-itself bugs this uncovered: `bytes[i]==b'{' &&
      bytes[i+1]==b'{'` mutated so the second read becomes `bytes[i]` again
      -- trivially true whenever the first check already passed -- turns
      every lone `{` into a false token start; the same pattern exists in
      `find_close`). `parse_var`'s and the token-name validator's `||`
      character-class checks pinned by accept/reject tests for `_`/`-`
      specifically (not just alongside an alphanumeric). `run`'s `(None,
      None, Some(s))`/`(None, None, None)` match arms covered by direct
      calls. The parent-directory-creation guard was extracted into its own
      `ensure_parent_dir_for_new_output` function (mirroring M11-5/6's
      pure-logic extractions) and tested directly, because testing it only
      through `run`'s public interface can't distinguish a broken guard from
      correct behaviour: the downstream `crate::cmd::write::run` →
      `atomic_write` also creates the destination's parent directory as its
      own safety net. One mutant accepted as equivalent: the inner `&&`
      (`!parent.as_os_str().is_empty() && !parent.exists()`) mutated to `||`
      has no distinguishing input, because `fs::create_dir_all` returns
      `Ok(())` as a no-op for both edge cases the guard exists to skip (an
      already-existing directory, confirmed via the existing idempotency
      behaviour of `create_dir_all`, and an empty path, confirmed directly:
      `create_dir_all(Path::new(""))` == `Ok(())` on this platform) --
      there is no input for which `&&` and `||` differ observably. **Real
      scoped `cargo mutants -f crates/tpu/src/cmd/render.rs` run (nextest
      tool): 184 mutants tested, 182 caught, 1 missed (documented above),
      0 unviable, 0 timeouts.**
- [x] M11-10: `crates/tpu/src/data_format.rs` (19 missed / 2 timeouts unique
      in the M11-1 baseline). **Complete.** `encode_base64_pem`'s manual
      `while pos < flat.len() { ...; pos = end; }` loop (its only timeout
      source) refactored to `flat.as_bytes().chunks(64)` per M11-4,
      eliminating the position-arithmetic mutation sites outright; already
      had extensive boundary tests (exact-48-bytes, 49-bytes-wraps, and
      others) from before this milestone, so this was a pure structural fix
      with no new tests needed. Added exact-value tests (no round-trips, per
      this item's original warning) for: `b64_val`'s value for every
      character class (`A`-`Z`, `a`-`z`, `0`-`9`, `+`, `/`); `b64_val`'s
      `b'=' => Err(...)` match arm against being deleted (asserted the
      *exact* padding-specific error message text, which differs from the
      generic-invalid-character arm's text); `decode_base64`'s per-chunk-
      index position arithmetic in its error messages (one test per chunk
      index 0-3, each with the invalid character in the *second* base64
      group so a `*` -> `/` mutation is distinguishable from `*` -> `+`,
      which only coincide at group 0); `encode_base64`'s RFC 4648 test
      vectors (`f`/`fo`/`foo`/`foob`/`fooba`/`foobar`). Seven mutants
      accepted as equivalent, all following one underlying principle: every
      `|` in this file's bit-packing code combines values already shifted
      into disjoint bit ranges (that is what a bit-packing shift *is*), so
      OR and XOR compute byte-for-byte identical results for every input --
      confirmed by hand-mutating `decode_hex`'s `(hi << 4) | lo` and
      re-running the file's full test suite (178 tests, all pass regardless
      of `|` vs `^`), then generalising to the other six `|` sites in
      `decode_base64` and `encode_base64` by the same reasoning rather than
      re-verifying each individually. **Real scoped `cargo mutants -f
      crates/tpu/src/data_format.rs` run (nextest tool): 268 mutants tested,
      246 caught, 14 missed (7 unique, matching the equivalence above
      exactly), 8 unviable, 0 timeouts.**

**Status:** ✅ Complete. Every item (M11-1 through M11-10) is done. M11-4
(the general loop-refactor principle) was applied four times total (M11-7
once, M11-8 three times), and deliberately *not* applied in M11-9, whose
loop index turned out to be genuinely load-bearing per M11-4's own
exception clause.

M11-2: `retry_io`'s three timeout mutants fixed via a bounded-timeout
background thread; `worker.rs`'s `write_response` and `main.rs`'s
`send_response`/`send_error`/`v.is_null()` fixed via direct in-memory-buffer
unit tests; `IoWorker::call` accepted as a documented `mutants::skip` (zero
already-caught mutants sacrificed). `try_call`'s `!have_worker` site, noted
at the time as the one undecorated holdout, turned out to be resolved for
free once M11-3 landed (see M11-2's update note and M11-5).

M11-3: `cargo-nextest` installed and configured (`.config/nextest.toml`,
`.cargo/mutants.toml`); `ci.yml` switched to `cargo nextest run` + `cargo
test --doc`; empirically validated that nextest's per-test termination
converts a genuine hang into a fast reported failure, and that `cargo
mutants --test-tool=nextest` runs cleanly end-to-end.

M11-5/6/7 (worker.rs, main.rs, lib.rs): re-verified via a real scoped
`cargo mutants` run against the fully-edited source: **138 mutants tested,
115 caught, 9 missed, 14 unviable, 0 timeouts.** All 9 remaining missed
mutants are individually documented as either mathematically equivalent
(disjoint-bitmask `|`/`^` swaps in `open_lock_file`, `WriteLock`/
`IoWorker`'s `Drop` impls given their fields' own drops, an empty-bodied
`apply_arg` branch, `is_worker_dead`'s current always-true semantics) or a
platform-coverage gap (the `#[cfg(not(windows))]` variant of
`open_lock_file`, uncompiled and therefore untestable on this Windows
machine). `TextLayout::analyze` was refactored to an iterator-driven walk
(M11-4), eliminating its timeout mutants outright.

M11-8 (encoding.rs): re-verified via a real scoped `cargo mutants` run:
**122 mutants tested, 112 caught, 0 missed, 10 unviable, 0 timeouts** — a
perfect score, up from 24 missed / 4 timeouts in the M11-1 baseline. Three
functions (`replace_u16_pairs`, `normalize_bytes_to_lf`,
`normalize_u16_to_lf`) refactored to iterator/`chunks_exact`-driven walks
per M11-4, eliminating their timeout mutants outright; a fourth
(`apply_line_ending_to_all`) pinned with a direct test. All four functions
had zero direct unit tests before this item.

M11-9 (cmd/render.rs): re-verified via a real scoped `cargo mutants` run:
**184 mutants tested, 182 caught, 1 missed, 0 unviable, 0 timeouts.** The
one missed mutant (`&&` vs `||` on a directory-creation guard) is
documented as equivalent given `fs::create_dir_all`'s no-op behaviour on
both edge cases the guard exists to skip. Along the way, found and fixed a
real "compare a byte to itself" bug pattern (a `+1` index mutated so a
lookahead re-reads the byte just matched, which is trivially still true) in
both `render_str` and `find_close`.

M11-10 (data_format.rs): re-verified via a real scoped `cargo mutants` run:
**268 mutants tested, 246 caught, 14 missed (7 unique), 8 unviable, 0
timeouts.** All 7 unique missed mutants are `|` vs `^` swaps in bit-packing
expressions, all following the same disjoint-bit-range equivalence
established in M11-7/M11-8's lock-file work, spot-verified once by hand
(178 tests unaffected) rather than re-verified per site.
`encode_base64_pem`'s manual position-tracking loop (its only timeout
source) refactored to `chunks(64)` per M11-4.

M11-1 (before/after comparison across all six Tier 2 files): **before**
849 mutants tested, 185 missed, 596 caught, 32 unviable, 36 timeouts;
**after** (same six files, now via nextest per M11-3) 716 mutants tested,
16 missed, 656 caught, 38 unviable, **0 timeouts**. Missed mutants down 91%
(185 → 16); every one of the 36 timeouts is gone. All 16 remaining missed
mutants are individually documented in M11-5 through M11-10 as
mathematically/behaviourally equivalent or a platform-coverage gap — see
M11-1 for the full per-file table.

Full workspace: `cargo nextest run` 3 823 tests passed, 0 skipped, ~35s;
`cargo fmt --check` and `cargo clippy --all-targets` both clean (no new
warnings from this milestone's work; a handful of pre-existing warnings
elsewhere in the workspace, unrelated to these changes, remain untouched).

---

## Milestone 12 — Mutation-testing coverage: Tier 3 (remaining files)

**Theme:** the fourteen lowest-count files from the same 2026-09-08
`cargo mutants` run (87 missed mutants combined, 1–14 per file). Individually
low-impact, but the same two families as Tier 2 — boolean-logic argument
combinations and boundary conditions on counting/limiting logic — recur
throughout, so the fixes are mechanically similar once the Tier 2 patterns
are established.

- [x] M12-1: `crates/tpu/src/main.rs` (14 missed). **Complete.** Extracted
      `WORKER_STACK_SIZE`, `eol_advisory_enabled_for`, and
      `eol_normalize_requested` as directly-testable pure functions (M11-4
      style). The three `if diff && !diff_buf.is_empty()` sites (write/
      replace/edit) needed `--message-format=json` tests specifically,
      because `HumanOutput::emit_json` only ever writes the (possibly-empty)
      `rendered` field directly to stdout — an empty diff collapses to zero
      bytes either way, making the `&&`/`||` mutation unobservable in human
      mode — while `JsonOutput::emit_json` always writes a full NDJSON
      envelope regardless of content emptiness. Discovered and filled a
      complete gap: zero `doctor` CLI integration tests existed at all;
      added six covering `format=json`, `--fix peel|all`, `--quiet`, and the
      clean/dirty exit-code boundary. Also discovered and filled a second
      gap: zero `setup --inject` CLI tests existed; added tests for all
      three verb outcomes ("block appended" / "already up to date" / "block
      replaced") pinning the `Commands::Setup` dispatch's `if !updated`
      against a `delete !` mutation. One mutant accepted as equivalent: the
      binary-mode `diff && !binary` guard's `&&`, mutated to `||`, is
      unobservable because `cmd::edit::run`'s binary branch unconditionally
      discards `diff_out` (`let _ = diff_out;`), so `diff_buf` stays empty
      regardless of whether it was ever `Some` — confirmed by tracing every
      downstream read site.
- [x] M12-2: `crates/tpu/src/test_fixtures.rs` (11 missed). **Complete.**
      Added direct value-assertion tests for `latin1_fragment()` and
      `double_cafe()` (previously only checked for UTF-8 validity, never
      their actual content) and two boundary tests for the file's private
      `b64()` decoder's short-chunk handling (`chunk.len() > 2` / `> 3`,
      pinning both the boundary and an adjacent `+`/`*`/`==`/`!=` cluster by
      forcing an out-of-bounds panic under the mutated comparison). Three
      mutants accepted as equivalent, all bit-packing `|`-vs-`^` swaps or a
      `>`-vs-`>=` boundary masked by a `.get(2)`/`.get(3)` `None` default:
      `b64`'s `(b[0]<<2)|(b[1]>>4)`, `(b[2]<<6)|b[3]`, and the `chunk.len() >
      2 && ...unwrap_or(&b'=') != b'='` guard (for `chunk.len()==2`, the only
      length where `>`/`>=` differ, the second half of the `&&` is masked to
      `false` by the `None` default either way).
- [x] M12-3: `crates/tpu/src/cmd/find.rs` (10 missed). **Complete.**
      Extracted `spec_is_glob` from `expand_paths_with_policy`'s inline
      4-way `||` chain (M11-4 style) with a direct per-metacharacter test.
      `run_single_file`'s separator/de-duplication logic needed three new
      tests discovered only via a real scoped re-run (the original fix
      pass under-covered this area): `-B`-alone (no `-A`) non-adjacent
      groups still need a `--` separator, pinning `lines_before > 0` against
      a `<` mutation (always false for a `usize`) that would otherwise be
      masked whenever `lines_after > 0` is also true; a bridging-gap case
      pinning the `.find(|(n,_)| ... *n > last)` computation of
      `first_to_emit` against `==`/`<` mutations (either wrongly falls back
      to the *current* match's own line number, which is far enough past
      `last` to trigger a spurious separator); and an older-context
      de-duplication case (`-B 4`, matches on lines 3 and 5) pinning the
      *emission* loop's identical `*n > last` check against a `<` mutation,
      which would re-emit an already-shown line as a duplicate while
      skipping the genuinely new one. One mutant accepted as equivalent:
      `first_to_emit`'s `>` vs `>=` — the before-context buffer is always a
      contiguous run of line numbers, so whenever `last` falls inside it,
      `>` finds `last+1` and `>=` finds `last` itself; both are `<= last+1`,
      so the separator check `first_to_emit > last + 1` is `false` under
      both interpretations and there is no reachable input where they
      diverge.
- [x] M12-4: `crates/tpu/src/cmd/edit.rs` (9 missed / 1 timeout). **Complete.**
      The timeout source (`line_range_to_source_bytes_with_encoding`'s
      manual `while i + newline_width <= view.bytes.len()` loop) needed no
      refactor: it was simply never exercised by any existing test, so the
      infinite loop a `+=`-style mutation would cause never manifested
      either way; it remains a plain loop but is now covered directly (see
      below), and cargo-nextest's per-test kill would catch a genuine hang
      regardless (M11-3). New direct tests: `run_line`'s `decoded.bom_len >
      0` (BOM preservation across an edit, pinning `>` against `<`, which is
      always false for a `usize`); the `total_lines` calculation's `else`
      arm (a file *without* a trailing newline), previously untested by any
      fixture, pinning both the `+ 1` and the `== b'\n'` mutations via an
      `--insert` at exactly `total_lines + 1`; the patch-overlap check's
      `w[0].end > w[1].start` at the exact byte-adjacent boundary (two
      deletes whose ranges touch but don't overlap); and
      `line_range_to_normalized_bytes`'s / `line_range_to_source_bytes_with_
      encoding`'s duplicated `start_line == 0 || end_line == 0` guard,
      pinned by a numeric start past an `EOF_SENTINEL` end on an empty file
      (only `end_line` resolves to `0`, so `&&` wrongly defers to the next
      check and reports the wrong line number in the error message).
      `line_range_to_source_bytes_with_encoding`'s UTF-16LE-vs-UTF-16BE
      newline-width `||` was tested directly (bypassing the always-UTF-8
      public wrapper) by hand-constructing a `View` over raw UTF-16LE bytes.
      `parse_line_range`'s `lo > hi` boundary pinned by an explicit
      equal-pair range (`"5-5"`).
- [x] M12-5: `crates/tpu/src/cmd/describe.rs` (9 missed). **Complete.** This
      module had zero tests of any kind (it's implemented but not yet wired
      into the CLI, `#[allow(dead_code)]`). Added a full `mod tests`:
      `LineEndingSeen::as_str`'s five outcomes ("None"/"LF"/"CRLF"/"CR"/
      "Mixed") each asserted individually by exact string (not just
      "not None"), which is what's needed to catch both a whole-
      function-replace mutant and a single-match-arm deletion (a deleted
      arm falls through to the `_ => "Mixed"` catch-all, which still looks
      like a plausible string unless every expected value is checked); and
      `run`'s `decoded.bom_len > 0` pinned the same way as M12-4's
      analogous case (BOM'd vs. plain file).
- [x] M12-6: `crates/tpu/src/cmd/head.rs` (8 missed). **Complete.**
      `run_lines`'s `is_last_selected = i + 1 == take` and `had_terminator =
      !is_last_selected || take < all_lines.len() || file_ends_with_newline`
      needed two precisely-chosen direct tests (no existing fixture lacked
      a trailing newline): requesting *all* lines of a file with no
      trailing newline (pins `==`/`+`/`!`/the `<` boundary all at once,
      since a mutated `is_last_selected` or a `<=` on `take < len` both
      wrongly add a terminator to the file's true last line) and requesting
      *fewer* lines than exist (pins the same `<` from the other side, and
      the `||` joining it to `!is_last_selected`, since only that middle
      term is true here). `run_bytes`'s `file_len == 0 || n == 0` guard is
      accepted as equivalent: memory-mapping a genuinely empty file with
      `memmap2` does not error on this platform (confirmed empirically — a
      direct test with the early return effectively disabled still passes,
      because `read_raw_bytes` succeeds with an empty `Vec` and the
      subsequent `take = n.min(0) = 0` truncation produces identical empty
      output either way).
- [x] M12-7: `crates/tpu/src/cmd/setup.rs` (7 missed). **Complete.** This
      module had zero CLI-level `setup --inject` tests at all. Added seven:
      the three verb outcomes ("block appended"/"already up to date"/"block
      replaced", also fixing M12-1's `main.rs` dispatch gap), an empty
      existing file (must not add a leading blank line), a file already
      ending in a single `\n` and one with no trailing newline at all (both
      must still get exactly one blank line of separator — pinning the
      inner `!out.ends_with('\n')` and outer `!out.ends_with("\n\n")` guards
      independently), and an out-of-order `END_MARKER`-before-`BEGIN_MARKER`
      file (must error, not silently "replace" — pinning the `e > b` match
      guard against a `replace guard with true` mutation). Two mutants
      accepted as equivalent: the parent-directory-creation guard's `!` on
      `if !parent.as_os_str().is_empty()`, redundant with `atomic_write`'s
      own parent-directory creation inside `cmd::write::run` (identical
      reasoning to M11-9's `render.rs` finding); and the `e > b` guard's `>`
      vs `>=`, unreachable because `BEGIN_MARKER` and `END_MARKER` are
      distinct fixed strings that can never be found at the same offset, so
      `e == b` is a structurally impossible input.
- [x] M12-8: `crates/tpu/src/cmd/write.rs` (7 missed). **Complete.**
      `detect_target`'s `source.bom_len() > 0` pinned the same way as
      M12-4/M12-5 (direct unit tests against a hand-constructed `View`/
      temp file, bypassing the fact that `write_bom` for `OutputEncoding::
      Preserve` only consults `source_had_bom` when a git-attributes
      `working-tree-encoding` is configured — untestable through `run`'s
      public interface outside a real git repo). `encode_new_file_content`'s
      `bom_policy == BomPolicy::Force` tested via `tpu create --utf8
      --bom=force`/`--utf8` (this function, not `run`, is `create`'s code
      path for brand-new files). Two `diff_out.is_some() && file.exists()`
      guards (in `run` and `run_binary`) needed a `--diff` invocation
      against a target that does not yet exist, which no prior test
      covered — without the guard, both would attempt `fs::read` on a
      nonexistent file and fail outright. One mutant accepted as
      equivalent: `run`'s inner `policy.reject_introduced_mojibake &&
      file.exists()` (the first, non-diff-related term of `need_old_bytes`)
      has no distinguishing input, because its only reader
      (`old_decoded`, used solely inside `if policy.reject_introduced_
      mojibake`) is gated by the identical flag, and `old_bytes.is_some()`'s
      other reader (the diff-emission `if let (Some(out), Some(old))`) is
      already forced by `diff_out.is_some()` regardless of this term.
- [x] M12-9: `crates/tpu/src/cmd/tail.rs` (4 missed). **Complete.** Same
      pattern as M12-6, simplified because `tail` always selects a suffix
      ending at the file's true last line (no `take < all_lines.len()`
      term needed): one direct test requesting fewer lines than exist in a
      file with no trailing newline pins `i + 1 == take` and `delete !`
      together. `run_bytes`'s `file_len == 0 || n == 0` is accepted as
      equivalent for the identical reason as M12-6's `head.rs` case.
      **Investigated and resolved a false lead:** a first verification pass
      (run concurrently with an unrelated, resource-heavy background
      mutants sweep) reported this file's other three `run_bytes` mutants
      (`135:17`/`135:27`'s `==`, `141:29`'s `-`) as `TIMEOUT` at the full
      258s budget. A clean, uncontended re-run resolved all three to
      `caught` with no timeouts at all — confirming the apparent hangs were
      system-load artifacts from running two `cargo mutants` invocations
      simultaneously, not a real infinite-loop risk in `redwing::
      materialize_range`. Lesson for future scoped runs: don't run more
      than one `cargo mutants` invocation at a time on this machine, even
      across disjoint file sets, or slow-but-finite mutated code paths can
      spuriously exceed the timeout budget under contention.
- [x] M12-10: `crates/tpu/src/walk.rs` (3 missed / 1 timeout). **Complete.**
      The timeout source (`CqItem::Terminal(_) => break` match-arm
      deletion, which would spin the `loop { match ring.wait_pop() {...} }`
      forever once the ring signals end-of-stream) needed no fix at all:
      cargo-nextest's per-test kill (M11-3) already converts the resulting
      hang into a fast, ordinary test failure — confirmed by a real scoped
      run showing `0 timeouts`. Two mutants accepted as equivalent, both
      requiring tracing into the `globazog` engine's internals rather than
      writing a test: `.result_shape(MetaMask::TYPE | MetaMask::REPARSE)`'s
      `|` (both `^` and `&` mutations) has no distinguishing input, because
      `Query::fetch_mask()` unions `result_shape` with
      `required_fields(&self.descend)`, and `descend` already includes a
      `Leaf::IsReparse` entry (added independently, to skip reparse points
      while walking directories) whose own `required_fields` mapping
      already forces `MetaMask::REPARSE` into the fetch mask regardless of
      `result_shape` — and separately, `entry_type`/`is_reparse` are always
      populated at the OS-directory-entry layer (`sys/win.rs`,
      `sys/linux.rs`) unconditionally, never gated by the fetch mask at
      all, only the "extra stat" fields (size/mtime/attrs) are; and the
      `CqItem::ContainerEnd(e) => { containers.remove(&e.id); }` match-arm
      deletion is a pure memory-bound optimisation (`ContainerId` is a
      monotonically-increasing `NonZeroU64` counter, so ID reuse — the only
      way a stale cached path could cause an actual correctness bug — would
      require exhausting a 64-bit counter within one process), with zero
      observable effect on any test of a realistic size.
- [x] M12-11: `crates/tpu/src/escape.rs` (2 missed / 6 timeout unique).
      **Complete.** All 6 timeout mutants (`decode`'s five `i += 1` arms
      and `decode_bytes`'s equivalent) needed no fix: they were already
      exercised by pre-existing tests (`decode_lf_escape`, `decode_tab_
      escape`, etc.), and cargo-nextest's per-test kill (M11-3) already
      converts the resulting hang into a fast failure — confirmed missing
      from a real scoped run entirely (0 timeouts). Added direct
      `to_string()` tests for all four `DecodeError` variants, asserting
      exact message text (not just non-empty), which is what's needed to
      catch a whole-function-`Display::fmt`-replace mutant that returns
      `Ok(Default::default())` (writes nothing, producing an empty string
      that a looser check wouldn't catch). One mutant accepted as
      equivalent: `decode_hex_digits`'s `value = (value << 4) | digit`, the
      same disjoint-bit-packing `|`-vs-`^` equivalence established
      throughout M11 (`value << 4` always has its low 4 bits zeroed by the
      shift, and `digit` is always a 4-bit hex nibble, so the two operands
      never share a set bit).
- [x] M12-12: `crates/tpu/src/cmd/validate.rs`, `crates/tpu/src/cmd/read.rs`,
      `crates/tpu/src/cmd/create.rs` (1 missed each). **Complete.** Three
      independent instances of the same `lo > hi` / `lo == 0 || hi == 0`-
      adjacent boundary pattern already fixed repeatedly across M11/M12:
      `validate.rs`'s `byte_slice` and `read.rs`'s `parse_bytes_arg` each
      needed one explicit equal-pair test (`byte_slice(bytes, 3, 3)` /
      `parse_bytes_arg("5-5")`) pinning `>` against `>=`.  `create.rs`'s
      `e.kind() == std::io::ErrorKind::AlreadyExists` (in the closure
      mapping `atomic_create_new`'s I/O error to a user message) could not
      be reached with a *non*-AlreadyExists error through `run`'s public
      interface alone — the earlier `try_exists()` advisory check intercepts
      the common case, and no other reachable I/O failure bypasses it — so
      the mapping closure was extracted into a directly-testable
      `map_atomic_create_error` function (M11-4/M12-4 style) and pinned with
      two direct calls using synthetic `std::io::Error` values of different
      kinds.

**Per-file scoped `cargo mutants` re-verification (nextest tool), each
against the fully-edited source:**

| File | Before (missed/timeout) | After (missed/timeout) |
|---|---|---|
| `main.rs` | 14 / 0 | 1 / 0 (equivalent) |
| `test_fixtures.rs` | 11 / 0 | 3 / 0 (equivalent) |
| `cmd/find.rs` | 10 / 0 | 1 / 0 (equivalent) |
| `cmd/edit.rs` | 9 / 1 | 0 / 0 |
| `cmd/describe.rs` | 9 / 0 | 0 / 0 |
| `cmd/head.rs` | 8 / 0 | 1 / 0 (equivalent) |
| `cmd/setup.rs` | 7 / 0 | 2 / 0 (equivalent) |
| `cmd/write.rs` | 7 / 0 | 1 / 0 (equivalent) |
| `cmd/tail.rs` | 4 / 0 | 1 / 0 (equivalent) |
| `walk.rs` | 3 / 1 | 2 / 0 (equivalent) |
| `escape.rs` | 2 / 6 | 1 / 0 (equivalent) |
| `cmd/validate.rs` | 1 / 0 | 0 / 0 |
| `cmd/read.rs` | 1 / 0 | 0 / 0 |
| `cmd/create.rs` | 1 / 0 | 0 / 0 |
| **Total** | **87 / 8** | **14 / 0** |

All 14 remaining missed mutants are individually documented above as
mathematically/behaviourally equivalent — no observable difference exists
between the original and mutated code for any reachable input. All 8
original timeout sources are gone: two were resolved for free by
cargo-nextest's per-test kill (M11-3, `walk.rs`'s `CqItem::Terminal`
deletion and `escape.rs`'s six manual-loop-counter mutants), and the one
new set of apparent timeouts encountered mid-milestone (`tail.rs`, 3
mutants) turned out to be a resource-contention artifact from running two
`cargo mutants` invocations concurrently, not a real hang — resolved to 0
timeouts on an uncontended re-run.

Verification was split across a comprehensive combined scoped run (all
fourteen files together, mirroring M11-1's methodology: 1,220 mutants
tested in 3h, 1,159 caught, 30 missed, 27 unviable, 4 timeouts) plus a
separate clean re-run of `cmd/find.rs` and `cmd/tail.rs` alone (186
mutants, 174 caught, 4 missed, 8 unviable, 0 timeouts) — the combined run's
snapshot of those two files predated the final round of test additions
made to them, and its concurrent unrelated load produced the `tail.rs`
timeout artifact described above. The per-file table and equivalence
documentation above reflect the final, clean numbers.

Full workspace: `cargo nextest run` all tests passed, 0 skipped; `cargo fmt
--check` and `cargo clippy --all-targets` both clean (no new warnings from
this milestone's work; the pre-existing `line_range_to_source_bytes`/`_
with_encoding` dead-code warning in `cmd/edit.rs`'s `--bin` build predates
this session — that pair is genuinely unused by any production code path,
only by its own test suite, and `pub fn line_range_to_source_bytes` is kept
as a documented, stable helper for future callers).

**Status:** ✅ Complete. Every item (M12-1 through M12-12) is done. Missed
mutants across all fourteen Tier 3 files down from 87 to 14 (all
individually documented as equivalent), and every one of the 8 original
timeouts is gone (2 resolved for free by cargo-nextest's per-test kill from
M11-3, 6 likewise, and one newly-encountered set of 3 apparent timeouts
traced to a contention artifact and confirmed absent on an uncontended
re-run).

---

## Milestone 13 — Mutation-testing coverage: `tpu-mcp/src/tools.rs` carry-over

**Theme:** the one piece of Tier 1 left unfinished when Milestone 11 was
scoped down to Tier 2 (see Milestone 11's intro). `tools.rs`'s *pure* helper
functions are already fully covered (done, no further action needed):
`tool_names`, `diff_separator`, `is_binary_selector`, `hex_nibble`,
`is_windows_drive_path`, `flatten_validate_pairs`,
`mojibake_policy_from_args`, `ServerConfig::to_wire`/`from_wire` round-trips,
`current_version`. What remains is the harder, more expensive-per-mutant
category deliberately deferred out of Milestone 11: `&&`/`||`/`!`
argument-combination logic inside the MCP `call_*` endpoint handlers
themselves, which can only be exercised at the MCP-protocol level (spinning
up the `tpu-mcp` worker and sending real requests), not via a plain unit
test — the same reason `tpu-mcp/src/main.rs` and `worker.rs`'s protocol
layer in Milestone 11 needed a different testing style than the `tpu` CLI's
Tier 2/3 files.

Unlike Tier 2/3's `tpu` CLI files, `tools.rs`'s own `integration_tests`
module calls `super::call(name, args, &ServerConfig)` **directly in-process**
(no subprocess spawning), making its scoped `cargo mutants` runs far cheaper
(~13 minutes for 261 mutants, vs. multi-hour runs for the CLI's own
integration-test files) — so no MCP-worker-level tests were actually needed;
plain in-process `call()` tests sufficed throughout.

**Baseline:** 67 missed mutants (2026-09-08 `cargo mutants` run, Tier 1,
already deduplicated), all in `crates/tpu-mcp/src/tools.rs`.

**Result:** 261 mutants in a final scoped verification run (some baseline
line numbers had drifted / some mutants were reclassified as unviable by a
newer compiler): **2 missed (both confirmed equivalent), 219 caught, 36
unviable, 2 genuine timeouts (documented, same category as M11/M12
precedent), and 2 additional timeouts that were confirmed flaky re-runs at a
tighter-than-usual auto-computed timeout** (re-verified caught with a manual
`--timeout 60`; not a real gap — see M13-Misc below).

- [x] M13-1: `call_write_file` (2 missed). **Complete.** Added
      `write_file_diff_true_shows_diff_on_real_change` (pins `if diff &&
      !diff_buf.is_empty()` against a `delete !` mutation) and
      `write_file_diff_true_on_no_op_write_has_no_spurious_blank_line` (pins
      the same guard against `&&`-to-`||`: writing byte-identical content
      with `diff:true` leaves `diff_buf` completely empty — proven directly
      against `tpu::cmd::write::run`'s `emit_text_diff` by the pre-existing
      `write_text_diff_no_change_is_empty` test in
      `crates/tpu/src/cmd/write.rs` — so only a genuinely no-op write, not
      simply omitting `diff:true`, can distinguish `&&` from `||` here).
- [x] M13-2: `call_replace_in_file` (8 missed). **Complete.** Added 5 tests:
      `replace_diff_true_shows_removed_lines_not_just_changed_region_echo`
      (kills two mutants at once — the `diff_out` capture guard's `diff ||
      dry_run` against `&&`, and the echo-source guard's `delete !` — by
      checking for a removed `-` line, which only the real unified diff
      shows, never the changed-region echo); `replace_count_only_skips_
      write_lock_wait` and `replace_dry_run_only_skips_write_lock_wait`
      (each needed separately, since neither alone catches both `delete!`
      variants on `!count && !dry_run`); `replace_real_write_cleans_up_
      stray_bak_file` (kills both `wrote` calculation mutants at once); and
      `replace_diff_true_on_textually_noop_match_still_shows_changed_region_
      echo` (pins the echo-source guard's `&&`-to-`||` from the opposite
      direction of the first test: a pattern/replacement pair that are
      textually identical, e.g. `pattern:"foo", replacement:"foo"`, still
      matches — so `regions` is non-empty and the changed-region echo has
      content to show — but the whole-file unified diff comparing old/new
      *bytes* is empty since nothing actually changed, distinguishing
      `diff && !diff_buf.is_empty()` from `diff || !diff_buf.is_empty()`).
- [x] M13-3: `call_edit_file` (4 missed / 1 confirmed equivalent).
      **Complete.** Added
      `edit_file_diff_true_shows_diff_on_real_change` and
      `edit_file_diff_true_on_textually_noop_splice_has_no_spurious_blank_
      line` (same pairing pattern as M13-1/M13-2: edit ops are
      unconditional — no match/no-match semantics — so a `splice` whose
      `data` is byte-identical to the line(s) it replaces still "succeeds"
      but leaves `diff_buf` empty despite `diff:true`, distinguishing the
      outer echo-source guard's `&&` from `||`). One mutant accepted as
      equivalent: the `diff_out` capture guard `diff && !binary` mutated to
      `diff || !binary` — confirmed by hand-mutating the guard in isolation
      and running the full `call_edit_file` test group, which all still
      passed. In binary mode, `cmd::edit::run` unconditionally discards
      `diff_out` (`let _ = diff_out;`), so an over-eager capture has no
      effect there; in text mode, the *same* outer `diff &&
      !diff_buf.is_empty()` guard a few lines later re-gates on the
      original `diff` flag directly, so a needlessly-populated `diff_buf`
      (when `diff:false`) is simply never read regardless.
- [x] M13-4: `call_find` (7 missed). **Complete.** Added 5 tests:
      `find_no_match_produces_no_spurious_blank_line`, `find_with_match_
      produces_no_extra_blank_line` (separator content-empty guard, both
      `delete!` and `||` variants), `find_walk_warning_appears_in_each_
      file_progress_detail` and `find_walk_warning_summarized_under_
      summary_progress_detail` (the separator `.find()` comparison mutants;
      the Summary-progress-detail case needed a hand-built `ServerConfig`
      since the shared `call()` test helper hardcodes `EachFile`), and
      `find_no_warnings_under_summary_progress_detail_omits_message`
      (`warnings`-non-empty `delete!`). Triggering a genuine walk warning
      (not a hard error) required a multi-path array with one real matching
      file and one nonexistent *literal* file, since a single nonexistent
      glob spec is a fatal error under `expand_paths_with_policy`'s
      zero-matches rule, not a soft per-file warning.
- [x] M13-5: `call_render_file` (4 missed). **Complete.** Added
      `render_file_invalid_vars_key_character_is_rejected` and
      `render_file_vars_key_with_underscore_and_dash_accepted`. The first
      test needed a redesign after an initial version (`template:
      "{{BAD KEY}}"`) was found to be masked by `render_str`'s own,
      independent token-name validation (which rejects any `{{...}}`
      placeholder containing a space) — since that error also contains the
      substring "may only contain", both the correct and the `k.is_empty()
      && ...` mutated code paths produced an error that satisfied a
      loose `contains()` assertion, hiding the mutation entirely. Fixed by
      using a template with **no placeholders at all**: the bad key
      (`"BAD KEY"`) is present in `vars` but never referenced, so the
      *only* possible source of an error is the vars-key guard itself,
      correctly distinguishing `||` from a `&&` mutation.
- [x] M13-6: `call_count_file` (3 missed). **Complete.** `emit_*`/
      `standard_metric_names` only control output *routing* — `tpu::cmd::
      count::run` receives the raw `lines`/`words`/`chars`/`bytes` flags
      directly, not the derived `emit_*` values — so a bug in `any_standard
      = lines || words || chars || bytes` is only observable via a pattern
      label that collides with an unrequested standard-metric name. Because
      `&&` binds tighter than `||` in Rust, cargo-mutants' single-operator
      substitution does **not** regroup the whole chain left-to-right as
      naive reasoning would suggest (confirmed by reading the actual
      generated `.diff`, not the textual mutant description): mutating the
      *first* `||` gives `(lines && words) || chars || bytes`; the *second*
      gives `lines || (words && chars) || bytes`; the *third* gives `lines
      || words || (chars && bytes)`. Each needs `lines`/`words`/`chars`
      (respectively) to be the *only* true flag for the surrounding chain's
      truth value to actually depend on the mutated sub-term — added three
      tests, one per operator: `count_file_only_lines_requested_excludes_
      other_standard_names_from_routing` (a "chars"-labeled colliding
      pattern), `count_file_only_words_requested_excludes_other_standard_
      names_from_routing` ("bytes"-labeled), and `count_file_only_chars_
      requested_excludes_other_standard_names_from_routing`
      ("lines"-labeled).
- [x] M13-7: `call_copy_file` (3 missed). **Complete.** Added
      `copy_file_warning_log_is_not_corrupted_by_writer_byte_count`, pinning
      `SharedWriter::write`'s return-value mutants (`Ok(0)`/`Ok(1)` instead
      of `Ok(b.len())`). `extend_from_slice` always appends the full slice
      regardless of the reported count, so the mutation is only observable
      via `write_all`'s retry semantics: `Ok(1)` causes `write_all` to
      retry with the (already-fully-appended, unshrunk) remaining slice,
      duplicating overlapping bytes — a genuinely corrupted/duplicated log
      buffer. Triggering a `shell.warn()` call portably (no symlinks,
      which require Developer Mode / admin rights on Windows) used a
      pre-created plain **file** at a path where the walk needs to create a
      **subdirectory**, so `fs::create_dir_all` deterministically fails
      with `AlreadyExists` and the `Entry::Dir` arm's warning fires.
- [x] M13-8: `render_changed_regions` (3 missed / 2 timeout). **Complete.**
      Added `render_changed_regions_exact_max_line_bytes_not_truncated`
      (the `line.len() > MAX_ECHO_LINE_BYTES` boundary) and `render_
      changed_regions_truncates_at_char_boundary_not_mid_multibyte_char`
      (constructs a line whose exact byte-500 cut point falls inside a
      multi-byte UTF-8 character, forcing the `boundary -= 1` backward scan
      to actually execute and proving it doesn't panic on a mid-character
      split). The `boundary -= 1` loop counter's `-=`-to-`/=` mutation
      remains a documented TIMEOUT: the test constructs the exact scenario
      that would hang under this mutation (and would be caught by
      cargo-nextest's own 60s kill), but `cargo-mutants`' auto-computed
      timeout for this fast-testing file (~24s) is short enough that the
      external budget expires before nextest's kill would — same category
      as M11/M12's TIMEOUT precedent, confirmed by a manual `--timeout 60`
      re-run which reproduced the identical timeout (a real hang, not
      flakiness).
- [x] M13-9: `stamp_and_verify` (3 missed). **Complete.** Extracted the
      inline `actual_ms.abs_diff(now_ms) > 10` tolerance check into a
      standalone, directly-testable `mtime_drift_exceeds_tolerance(actual_ms,
      expected_ms) -> bool` (M11-4-style extraction — forcing an exact 10ms
      real-filesystem mtime drift is impractical). Added
      `stamp_and_verify_zero_delay_never_opens_file_for_writing` (pins
      `delay_ms == 0` against a `!=` mutation, via a read-only file that
      would error if ever opened for writing) and `mtime_drift_exceeds_
      tolerance_boundary` (exact `> 10` boundary on the extracted function).
- [x] M13-Misc: Newly-discovered scope beyond the original 9-item
      breakdown, found only via the real scoped baseline run (not visible
      from a static read of the missed-mutant list alone): dispatcher
      match-arm deletes for `"tpu_read_file_binary"`/`"tpu_validate_file"`
      in `call()`; `eol_write_override`'s whole-function-replace-with-
      `Ok(None)`; `call_read_file_binary`'s hash-branch `delete!`;
      `call_append_file`'s `changed` calc `delete!`. Added
      `dispatcher_routes_tpu_read_file_binary`, `dispatcher_routes_tpu_
      validate_file`, `read_file_binary_with_hash_returns_hashes_json`,
      `write_file_explicit_line_ending_override_is_honoured`, and `append_
      file_diff_true_reports_changed_on_real_append`.

      **Timeout sources eliminated by refactor:** `normalize_bytes_to_lf`
      and `percent_decode_path` both had manual-index loop-counter mutants
      (`i += N` → `*=`/`/=`) capable of genuine infinite loops. Refactored
      both to eliminate manual indices entirely — `normalize_bytes_to_lf`
      now uses a `Peekable` iterator; `percent_decode_path` now uses
      slice-shrinking via `split_first()` — guaranteeing per-iteration
      progress regardless of any mutation to the (now-removed) advancement
      arithmetic. This also incidentally eliminated `hex_nibble`'s
      whole-function-replace timeout mutants, since the fallback path is
      now unconditionally progress-guaranteed. Added `decode_pattern_arg_
      returns_the_given_pattern_verbatim` for that function's separate
      whole-function-replace mutant (its own residual TIMEOUT — see below
      — traces to `tpu::cmd::replace::run`'s pre-existing empty-pattern
      handling, outside `tools.rs`'s control).

      One mutant accepted as equivalent post-refactor: `percent_decode_
      path`'s `hi << 4 | lo` bit-packing, mutated to `^` (XOR). `hi` and
      `lo` are both 4-bit nibbles (0–15) occupying disjoint bit positions
      once `hi` is shifted left by 4, so OR and XOR are mathematically
      identical for every possible input — confirmed no reachable input
      diverges. (A second mutation at the same site, `|` → `&`, which
      would zero every decoded byte, is already caught by existing tests.)
- [x] M13-10: Final verification — a scoped `cargo mutants -f
      crates/tpu-mcp/src/tools.rs` run confirmed 261 mutants: 2 missed
      (both confirmed equivalent, see M13-3 and M13-Misc above), 219
      caught, 36 unviable, and 2 genuine TIMEOUTs (`decode_pattern_arg`,
      `render_changed_regions`'s `boundary -= 1`, both re-confirmed as real
      hangs — not flakiness — via a manual `--timeout 60` re-run). Two
      *additional* TIMEOUTs seen in the first pass of this same run
      (`ServerConfig::to_wire`/`from_wire`, mutated to `Default::default()`)
      were confirmed **flaky, not real gaps**: re-run individually with
      `--timeout 60`, both were caught in well under that budget by the
      pre-existing `server_config_wire_round_trip_*` tests. The file's
      fast own-test baseline (~4s) makes cargo-mutants' auto-computed
      per-mutant timeout (~24s) tight enough that transient system load
      (e.g. several `cargo mutants` runs executed back-to-back in the same
      session) can occasionally starve an otherwise-fast, already-covered
      mutant past that budget — a timing artifact of the verification
      environment, not a test-coverage gap.

**Approach (carried over from M11/M12 methodology, refined this milestone):**
get a fresh deduped missed-mutant list, read each site, prefer direct
in-process `call()` tests (cheap for this file specifically), extract inline
logic into pure functions when direct testing is impractical, refactor away
manual-loop-counter patterns entirely when the file's own tests are fast
enough that cargo-mutants' auto-timeout is tight (nextest's kill may not
have time to "win" the race), and — the key lesson from this milestone —
**never assume how cargo-mutants regroups a boolean chain from the textual
mutant description alone; read the actual generated `.diff` file** under
`mutants.out/diff/*.diff` before writing a test or declaring a mutant
equivalent, since `&&` binds tighter than `||` in Rust and a single-operator
substitution can produce a materially different grouping than naive
left-to-right reasoning suggests.

**Status:** ✅ Complete.

