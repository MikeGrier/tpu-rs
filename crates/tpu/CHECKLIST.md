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
