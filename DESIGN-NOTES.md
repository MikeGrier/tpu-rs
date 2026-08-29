# tpu — Design Notes

## Overview

`tpu` (Text Processing Utility) is an encoding-aware, line-ending-neutral file tool
designed for both interactive command-line use and agent (Copilot MCP) use.  It bridges
the gap between files stored in arbitrary encodings and tools that expect clean UTF-8
input/output.

---

## CLI Argument Parsing — `clap` crate

All `tpu` subcommands use `clap` (with its `derive` feature) for command-line argument
parsing.  `clap` is the standard Rust ecosystem choice and is already a workspace
dependency.

---

## File I/O — harrier + redwing

All file I/O goes through `harrier` (encoding detection, LF-normalised views, BOM
awareness) and `redwing` (byte-rope for atomic splicing).  Direct use of
`std::fs::write` on file content is prohibited in `tpu`.

### IoMode: mmap vs buffered (2026-04-24)

`tpu` supports two I/O modes for reading files into redwing branches:

| Mode | Mechanism | When to use |
|---|---|---|
| `IoMode::Mmap` (default) | `memmap2::MmapOptions` + `make_thicket_from_mmap` | CLI usage, normal invocations |
| `IoMode::Buffered` | `std::fs::read` + `make_thicket_from_bytes` | MCP server usage |

The CLI passes `Mmap`; the MCP server passes `Buffered`.

**Rationale:** Windows Defender's real-time protection minifilter can terminate
LLVM-built processes that create memory-mapped file regions in rapid succession.
The MCP server drives tpu library functions rapidly and repeatedly, which triggers
Defender's heuristics and causes it to kill the process.  Normal interactive CLI
usage does not exhibit this pattern, so mmap remains the default for the CLI.

The `open_as_branch` helper in `lib.rs` centralises the mode dispatch so individual
commands do not contain branching logic for I/O mode.

This ensures:

- Any encoding detectable by harrier (UTF-8, UTF-16 LE/BE, Windows-1252, Shift-JIS, …)
  is handled transparently.
- The logical content seen by pattern-matching and replacement code is always LF-only,
  so callers never write patterns that account for CRLF.

---

## Atomic Write Pattern

Whenever `tpu` modifies a file it follows a safe, atomic write sequence:

1. Write new content to a temporary file in the **same directory** as the target (same
   filesystem, avoids cross-device `rename` failure).
2. Rename the original file to `<file>.bak`.
3. Rename (persist) the temporary file to the original path.
4. On any failure after step 2, attempt to rename `<file>.bak` back to the original
   path so the previous content is not lost.

### .bak retention

The `.bak` file is intentionally left on disk after a successful write.  It acts as a
one-level undo safety net.  A future `--no-bak` flag or a `tpu clean-bak <file>` command
may be added to remove it explicitly; no automatic deletion is planned at this time.
**Changing this retention policy is a user-visible behavioural change — document it in
these notes when it happens.**

---

## Default Encoding / Line-Ending Behaviour

The default for all subcommands is **preserve everything**:

| Property | Default |
|---|---|
| Character encoding | Preserve source encoding |
| BOM presence | Preserve source BOM |
| Line endings | Preserve dominant source convention (LF / CRLF / CR) |

---

## Encoding Output Normalisation (`--utf8`, `--bom`)

An optional `--utf8` flag may be added to subcommands that produce file output to force
re-encoding of the output as UTF-8 regardless of the source encoding.

When `--utf8` is active, a companion `--bom` option governs BOM handling:

| `--bom` value | Behaviour |
|---|---|
| `strip` (default) | No BOM in output |
| `preserve` | Include UTF-8 BOM only if the source file had a BOM |
| `force` | Always include a UTF-8 BOM |

The default `--bom` value is `strip`, i.e. no BOM unless the caller asks for one.

Supporting re-encoding to non-UTF-8 targets (e.g. `--windows-1252`) is explicitly
out of scope for the current design.  It may be revisited if a concrete need arises.

---

## Line-Ending Override (`--line-ending`)

The `--line-ending=<lf|crlf|cr>` flag is available on `write`, `replace`, and `edit`
(text mode only; conflicts with `--binary`).

### Invariants

1. **Encoding is unaffected.**  `--line-ending` changes only the newline denormalisation
   target.  The file's existing encoding (UTF-8, UTF-16LE/BE, Windows-1252, …) is
   still detected and preserved — or overridden independently by `--utf8`.

2. **Orthogonal to `--utf8` and `--bom`.**  All three flags are independent:
   `--utf8` controls re-encoding, `--bom` controls the UTF-8 byte-order mark, and
   `--line-ending` controls the newline byte sequence.  Any combination is valid.

3. **Normalised view is unchanged.**  The regex / match engine always operates on the
   LF-normalised view of the file regardless of the `--line-ending` setting.  The
   override is applied only during the final denormalisation step before writing.

4. **Absent = preserve.**  When `--line-ending` is absent the dominant line-ending
   detected from the existing file (or LF for new files) is used, exactly as before.

### Implementation notes

- In `write`: `detect_target()` returns the detected `LineEnding`; the override (if
  supplied) replaces it immediately after detection.
- In `replace`: the detected `line_ending` from `source.line_ending()` is stored as
  `detected_ending`; the override replaces it before the `denormalize_bytes` calls.
- In `edit` (text mode): same pattern — detected ending is shadowed by the override
  before `DenormaliseWriter` is constructed.
- In `tpu-mcp`: the `write_file` tool schema exposes `line_ending` as an optional enum
  string (`"lf"`, `"crlf"`, `"cr"`); when present it is passed to the library `run()`
  function as a `LineEnding` value.

### harrier primitives used — no harrier changes needed

The `--line-ending` flag was implemented entirely within `tpu` without modifying harrier.
This was possible because harrier already provides everything needed:

- **`LineEnding` enum** — the three variants (`LF`, `CRLF`, `CR`) map directly to the
  three `--line-ending` values.
- **`DenormaliseWriter`** — produces the chosen line ending during streaming writes.
- **`denormalize_bytes(buf, ending)`** — the batch equivalent used by `replace` after
  `redwing::materialize()`.

If a future caller ever needs to write lines with **heterogeneous endings** (e.g. preserving
the original per-line ending of each line independently), harrier's `Lines` iterator
already exposes per-line terminator information via the `Line::terminator` field.  The
current `tpu` design intentionally does not use this: a single uniform ending is applied
to the entire file on every write operation.  This keeps the output deterministic and
matches user expectation when `--line-ending` is given.

---

## Unicode Escape Convention (`readex` / `writeex`)

`readex` emits **7-bit clean ASCII on a single flat line**: every character outside the
printable ASCII range (U+0020–U+007E) is escaped, including all control characters and
line breaks.  The output contains **no literal newlines** — line breaks in the source
appear as `\n` escape sequences.  This is the format expected by Copilot and other
agent consumers that read the result as a string value.  It makes the output safe for:

- Command-line pipelines and shell variables on any platform
- JSON string values and other ASCII-only interchange formats
- Agent/tool output where 8-bit bytes can be misinterpreted

The escape codec uses the following rules (a strict subset of C++ / Rust string literal
syntax, well-understood and unambiguous):

| Sequence | Meaning |
|---|---|
| `\\` | Literal backslash (U+005C) |
| `\0` | NUL (U+0000) |
| `\t` | Horizontal tab (U+0009) |
| `\n` | Line feed (U+000A) |
| `\r` | Carriage return (U+000D) |
| `\uXXXX` | Unicode scalar value — exactly 4 hex digits, U+0000–U+FFFF |
| `\UXXXXXXXX` | Unicode scalar value — exactly 8 hex digits, U+010000–U+10FFFF |

All other bytes in the range U+0001–U+001F and U+007F are escaped as `\uXXXX`.
Printable ASCII (U+0020–U+007E, except backslash) is passed through unescaped.

`writeex` performs the exact inverse: it reads a 7-bit clean ASCII stream, validates and
unescapes the sequences above, then encodes the resulting Unicode text into the target
file's native encoding.  An invalid escape sequence is a hard error.

### Why not JSON `\uXXXX` only?

JSON allows only `\uXXXX` (BMP only) and requires surrogate pairs for supplementary
characters.  The `\UXXXXXXXX` form avoids surrogate pairs and is directly representable
as a Rust `char`, keeping the codec simple and round-trip correct for all Unicode scalar
values.

### `read` vs `readex` — when to use which

| Subcommand | Output | Use for |
|---|---|---|
| `read` | UTF-8 / LF (may contain multibyte sequences) | Piping to UTF-8-aware tools, agent file reads |
| `readex` | Single flat 7-bit ASCII line; line breaks → `\n` | Shell variables, Copilot/agent tool output, JSON embedding |
| `write` | Target file encoding + line endings | Agent file writes from UTF-8 input |
| `writeex` | Target file encoding + line endings | Agent file writes from escaped ASCII input |

---

## Multiline Regex Replace

The existing `replace` subcommand already normalises the file to LF before matching, so
a regex like `(?s)foo.*bar` spanning multiple lines works today.  However, usability
improvements are planned:

- Flag `--multiline` (short: `-m`) on `replace` to set the `(?m)` Rust regex flag
  automatically, making `^` and `$` match at LF boundaries within the file.
- The input text delivered to the regex engine is always the LF-normalised byte slice
  of the entire file.  Patterns may freely use `\n` to match line breaks.

This is a documentation and ergonomics clarification; the underlying normalisation
already handles it correctly.

---

## Replacement Capture-Group Expansion — Conditional on Group Presence

`tpu replace` expands capture-group references (`$0`, `$1`, `$name`, `$$`) in the
replacement string via `regex::bytes::Captures::expand` **only when the compiled
pattern actually contains at least one explicit capturing group**.  The presence of a
group is detected with `Regex::captures_len() > 1` (the count includes the implicit
whole-match group 0, so `> 1` means one or more explicit groups).

When the pattern has no groups the replacement bytes are written verbatim: the per-match
path pushes the raw bytes and the `--diff` preview path uses `regex::bytes::NoExpand`.

### Rationale

Historically the replacement was always run through `expand`, so a `$` that the user
intended literally (prices like `$5.00`, shell variables like `$HOME`, `${TOKEN}`
placeholders) was silently consumed as a group reference and lost — most painfully for
literal (non-regex) searches, where a capturing group is impossible by construction.  Tying
expansion to group presence makes the common "literal search, literal replace" case
behave as written while preserving back-references for patterns that opt into them.

### Consequence

`$0` is **not** interpreted as "the whole match" for a group-less pattern.  A caller who
wants the matched text echoed back must introduce a capturing group (e.g. wrap the
pattern in `( … )` and reference `$1`).  This is a deliberate, user-visible behavioural
change from the previous always-expand semantics.

Backslash-escape decoding (`\n`, `\t`, `\\`, …) is orthogonal and happens in the caller
before the bytes reach `run`, regardless of group presence — but the two front ends
differ in their default.  The CLI's `decode_replacement` decodes by default
(`sed`/`perl`/`ripgrep` convention, `--literal-replacement` / `-L` to opt out); the MCP
server writes `replacement` verbatim by default and only calls its
`unescape_replacement` when the caller passes `expand_escapes: true`.  See
`crates/tpu-mcp/DESIGN-NOTES.md` for why the MCP default was inverted.

---

## `tpu create` — Create-Only File Writes

`tpu::cmd::create::run` is a narrow wrapper over `tpu::cmd::write::run` that refuses to
clobber an existing file: it errors if the target path already exists and otherwise
delegates to `write::run` for the actual encoding-, line-ending-, and mojibake-aware
write.  A stranded `<file>.bak` (from an interrupted prior write) is recovered first via
`recover_stranded_backup`, so a half-completed write still counts as "exists".

### Motivation

Agents routinely need to create brand-new files, but the `write_file` name does not
telegraph that it also creates files, so Copilot struggles to anticipate the right tool
and sometimes reaches for shell redirection instead.  A dedicated create operation whose
name and contract match the intent removes that ambiguity.  Documenting `write` harder
was judged to be swimming upstream against the tool name.

### Scope

- New files default to UTF-8 with LF line endings; `output_encoding`, `bom_policy`, and
  `line_ending_override` override those defaults exactly as they do for `write::run`.
- A `tpu create` CLI subcommand mirrors the library contract (positional `FILE` and
  optional inline `DATA`, `--utf8`, `--bom`, `--line-ending`, `--allow-mojibake`), reading
  content from stdin when `DATA` is omitted.  This is how the MCP `tpu_create_file` tool
  drives it, and it lets humans opt into create-only semantics (and command-line line
  endings) rather than the clobbering `tpu write`.
- No `.bak` is produced (there is no prior content) and no diff is emitted (the whole
  content is the change).

### `main()` runs on a large-stack worker thread

The `tpu` binary parses its arguments and runs on a dedicated worker thread with a 16 MiB
stack rather than the OS main thread.  clap's derive-built command tree and its debug-time
`debug_assert` validation are stack-heavy, and on Windows the main thread's default stack
is only ~1 MiB.  As the subcommand set grew (adding `create` was the tipping point), a
plain `Cli::parse()` on the main thread began overflowing the stack in debug builds even
for `tpu --help`.  Running the whole program on a thread with a generous stack keeps the
tool robust as more subcommands are added; do not "simplify" this back onto the main
thread.

---

## Edit-Offset Composability

### Within a single `replace` call

`tpu replace` collects **all** match positions in a single pass over the LF-normalised
view, converting each match's byte range to **original-file source coordinates** via the
view's offset map before any edit is applied.  The resulting splice descriptors are then
sorted in **reverse source order** and applied one by one to a `redwing` branch fork.

Because splices are applied from the end of the file backwards, all lower-offset source
coordinates remain valid throughout the sequence — each splice only shifts the byte
ranges of content that has already been processed.  There is no need to adjust
coordinates after each edit.

Consequence: a replacement string that happens to contain the same text as the original
pattern does **not** cause subsequent splices to re-match it — all match positions were
already fixed before any write occurred.

### Across separate invocations (sequential composition)

Separate `tpu replace` or `tpu write` invocations are **sequentially composed**: each
invocation reads the file as it exists on disk at the time of the call, which already
includes all modifications made by previous invocations.  There is no parallel
composition model in `tpu`.

This means:

- A second `replace –A→B` call that is run after `replace –X→A` will match the `A`
  tokens introduced by the first call, not the original `X` tokens.
- The `.bak` file created by each invocation reflects the state **at the start of that
  invocation**, so there is one `.bak` per call — they are not cumulative.

This sequential model is intentional: it is simple, predictable, and composable in
shell pipelines or MCP tool chains without any coordination protocol.

---

## MCP Architecture — Direct Library Calls

The intended production consumer of `tpu` is an MCP server, not a human or Copilot
directly.  The chain is:

```
Copilot (structured JSON) → MCP server → tpu::cmd::*::run() → file on disk
```

Copilot never constructs a shell command.  It sends a typed JSON object — e.g.
`{ file, patches: [{range, format, data}], validate: [...], diff: true }` — to an MCP
tool.  The MCP server (`tpu-mcp`) extracts typed arguments from JSON and calls `tpu`
library functions directly (e.g. `tpu::cmd::read::run()`, `tpu::cmd::replace::run()`).
No subprocess is spawned and no CLI argument parsing (clap) is involved.

This architecture eliminates the entire class of bugs where argument values starting
with `--` are misinterpreted as CLI options by clap's argument parser.  Previously, the
MCP server serialised arguments into a `.rsp` response file and invoked `tpu @tmp.rsp`
as a subprocess.  Because clap sees all tokens identically regardless of quoting, any
value beginning with `-` could be misinterpreted as a flag.  Direct library calls bypass
this entirely — argument values are passed as typed Rust values, never as strings that
require parsing.

Consequences for the design:
- The MCP tool schema is the agent interface.  CLI syntax exists only for human operators.
- The `tpu` crate exposes `pub mod cmd` with a `run()` function per subcommand; these
  are the library entry points used by `tpu-mcp`.
- Output capture uses `tpu::output::human_output_to(writer)` to direct formatted output
  into an in-memory buffer rather than stdout.

---

## Dash-Prefixed Argument Values — Historical Note

When `tpu-mcp` invoked `tpu` as a subprocess via `.rsp` response files, argument values
starting with `-` or `--` could be misinterpreted as CLI options by clap.  Quoting in
the `.rsp` file did not help because `tokenize_rsp` strips quoting before clap sees the
tokens — clap has no memory of which tokens were quoted.

The `allow_hyphen_values = true` annotations on positional arguments in the clap CLI
definition remain in place for human operators who use `tpu` directly from the command
line.  However, the MCP server no longer exercises this code path — it calls
`tpu::cmd::*::run()` library functions directly, passing argument values as typed Rust
values that are never parsed as CLI flags.

This section is retained for historical context.  The architectural fix (direct library
calls) is documented in "MCP Architecture" above.

---

## MCP Boundary Line-Ending Normalization

All `tpu::cmd::*::run()` functions have an implicit contract: **text input must be
UTF-8 with LF-only line endings**.  The `DenormaliseWriter` used internally by write,
replace, edit, and append only recognizes bare `\n` bytes — it forwards `\r` verbatim
and then substitutes the `\n` that follows.  Consequently, if text content arrives
containing CRLF:

- **LF-target file**: `\r\n` → `\r` (verbatim) + `\n` (substituted to LF) = `\r\n`
  — CRLF is injected into an LF file, producing mixed line endings.
- **CRLF-target file**: `\r\n` → `\r` (verbatim) + `\r\n` (substituted to CRLF) =
  `\r\r\n` — a corrupted triple.

### The problem source

Copilot and other MCP clients send text content as JSON strings.  The JSON specification
does not require any particular line-ending convention, and in practice Copilot often
sends CRLF-terminated text — especially on Windows, where the editor's clipboard or
internal buffers may use CRLF.  This is not a bug in the client; the MCP server must be
defensive.

### The fix: normalize at the MCP boundary

`tpu-mcp` normalizes every text value to LF **before** passing it to any tpu library
function.  Two helpers handle this:

- **`normalize_to_lf(s: &str) -> Cow<str>`** — for `content` and `replacement` string
  parameters.  Returns a borrow (zero-cost) when the input already contains no `\r`.
- **`normalize_bytes_to_lf(bytes: Vec<u8>) -> Vec<u8>`** — for edit-op data that has
  already been extracted as `Vec<u8>`.  Only applied in text mode; binary edit ops are
  not modified.

This normalization is applied at the outermost boundary (the `call_*` functions in
`tools.rs`) rather than inside the tpu library.  The tpu library's contract of
"LF-only input" is unchanged; the MCP server just ensures it is honored.

Affected call sites:
- `call_write_file` — normalizes `content` before `write::run()`
- `call_replace_in_file` — normalizes `replacement` before `replace::run()`
- `call_edit_file` — normalizes Insert/Splice `data` bytes (text mode only)
- `call_append_file` — normalizes `content` before `append::run()`

The MCP tool schema descriptions document that CRLF/CR in input text is normalized to
LF before processing.

### Why not proactively normalize entire files?

`replace` and `edit` operate on raw source bytes for unmodified regions — only the
replacement or inserted data passes through denormalization.  A file with pre-existing
mixed line endings will remain mixed after a replace or edit (unless `line_ending` is
explicitly specified, which triggers a whole-file normalization pass in `replace`).

We deliberately chose **not** to add automatic whole-file line-ending normalization as
a side effect of every edit.  Reasons:

1. **Minimal surprise.**  If the caller asks to replace "foo" with "bar", the diff
   should show exactly that change.  Silently normalizing every line ending in the file
   would produce a noisy diff that touches lines the caller never asked to change,
   making code review harder.

2. **Already solvable when desired.**  The `line_ending` override parameter forces
   whole-file normalization in `replace`.  Callers that want to fix mixed endings can
   pass `"line_ending": "crlf"` (or `"lf"`) explicitly.

3. **Boundary normalization prevents *creating* new mixed endings.**  With
   `normalize_to_lf` in place, tpu-mcp will never introduce mixed endings going
   forward.  The only remaining source is files that already had mixed endings before
   tpu touched them.

4. **Pre-existing mixed endings are a git/editor concern.**  Files with mixed endings
   typically got that way from editor misconfiguration or bad merges.  Fixing them
   silently as a side effect of an unrelated edit obscures the real change in version
   control.

If whole-file normalization is ever needed as a first-class action, the right form is
a separate explicit tool (e.g. `tpu normalize` or a `tpu_normalize_line_endings` MCP
tool) rather than an implicit side effect.

### Future: mixed line ending detection and user-prompted normalization

The `describe` command already performs a full scan and reports `"Mixed"` when a file
contains two or more line ending styles (LF, CRLF, CR).  A natural extension would be
to surface this to the user after write operations and offer to normalize.

MCP is strictly request-response JSON-RPC — the server cannot pause mid-operation to
present a choice UI to the client.  This rules out an interactive "normalize now?"
prompt from within a tool call.

The viable path, if we pursue this later, is:

1. **Advisory warnings in tool results.**  After a write/replace/edit/append completes,
   run the mixed-endings check.  If mixed, include a warning in the JSON result
   (e.g. `"warning": "File has mixed line endings. Call tpu_normalize_line_endings to
   fix."`).  Copilot would see this and could relay it or act on it autonomously.

2. **Dedicated `tpu_normalize_line_endings` tool.**  Takes a file path and optional
   target line ending (defaulting to the file's dominant style via majority vote).
   Normalizes the entire file and reports what changed.

These two together give a clean experience: tools surface the problem, a dedicated tool
fixes it, and no implicit side effects occur.  Not implementing now — the boundary
normalization already prevents *creating* new mixed endings; this would only address
pre-existing ones.

---

## Binary Data Encoding — Symmetric Input/Output Formats

`tpu` defines a symmetric set of formats for moving binary data between the CLI/MCP
layer and raw file bytes.  The same three format names are used for both input
(`--data-format` on `write`/`writeex`) and output (`--output-format` on `read`/`readex`).

### `encoded` — 7-bit clean, human-readable

**Output (`read --output-format=encoded`):** printable ASCII (U+0020–U+007E) passes
through unchanged; backslash is emitted as `\\`; all other bytes are emitted as `\xHH`
(uppercase hex).  No trailing newline is added.  The result is a single flat ASCII line
safe for shell variables, JSON string values, and agent tool output.

**Input (`write --data-format=encoded`):** the inverse — parses the tpu escape codec
(`\xHH`, `\uXXXX`, `\n`, `\r`, `\t`, `\\`, printable ASCII) and writes the resulting
bytes to the file.

Human-only on the raw command line (backslash sequences require double shell-escaping).
Via `.rsp` or MCP, fully safe because no shell quoting is involved.

### `hex` — uppercase pairs with `-` separators

**Output:** every byte is emitted as an uppercase two-digit hex pair separated by `-`
(e.g. `4D-5A-00-00`).  No trailing separator or newline.

**Input:** accepts the same format; `-` separators are optional and may be omitted
(e.g. `4D5A0000` is equivalent).  Case-insensitive.  Odd number of hex digits after
stripping dashes is an error.

The dash-separated output form is chosen for readability when scanning byte sequences
manually.  The decoder's tolerance of the no-dash form means the output of one `tpu`
command can always be fed directly into another.

### `base64` — PEM-body format

**Output (`read --output-format=base64`):** standard RFC 4648 base64 (standard alphabet,
`=` padding), line-wrapped at exactly 64 characters per line, each line terminated with
`\r\n`.  No PEM header or footer (`-----BEGIN...-----`) is included — the output is the
bare base64 body only.  This matches the body section of PEM-encoded certificates and
keys, making it compatible with tools that consume PEM body content.

**Input (`write --data-format=base64`):** accepts:
1. Contiguous base64 with no whitespace (original behaviour).
2. PEM-style line-wrapped base64 (`\r\n` line endings, 64-char lines) — strip `\r` and
   `\n` before decoding.
3. Mixed whitespace (spaces, tabs, line endings) — already stripped by the existing
   decoder; verify this covers PEM line endings.

The 64-character line width is the PEM standard (RFC 7468).  `\r\n` line terminators
are used on output rather than `\n` so that the output is a valid PEM body even on
Unix hosts — PEM is defined with CRLF line endings.

### Round-trip guarantee

For any byte sequence `B`:
```
tpu read --binary --bytes=0-N --output-format=FMT  →  S
tpu write --binary --data-format=FMT FILE          ←  S (stdin or --data S)
```
yields the original bytes `B` for all three formats.  This round-trip is tested by the
MG-7 integration tests.

### MCP usage of output formats

The MCP `read_file` tool always passes `--output-format=hex` or
`--output-format=base64` so that the server receives a plain ASCII string it can embed
directly in a JSON response without binary-in-JSON concerns.  `encoded` is available
for MCP use but `hex` or `base64` are preferred since they have no escape sequences that
could confuse a JSON parser or intermediate log.

---

## Shell Abstraction — Separating Status from Structured Output

Cargo's architecture cleanly separates two output concerns that are easy to conflate:

| Stream | Purpose | Format | Destination |
|---|---|---|---|
| `Shell` | Status, progress, warnings, errors | Always human-readable text, colour-aware | stderr |
| Data pipeline | Payload: file content, diffs, diagnostics | Format-controlled (`--output-format`) | stdout |

`tpu` adopts the same split.  The invariant is:

> **Stdout contains only payload bytes.  Stderr contains only human-readable messages.**

This invariant is what makes the MCP integration reliable: the server reads stdout as a
structured value and can ignore stderr entirely (or relay it to a log).  If errors and
payload are mixed on stdout the server must parse around noise, which is fragile.

### `Shell` type

`Shell` is a struct (not a global) defined in `src/shell.rs`.  It holds:
- A `Box<dyn Write>` for the error stream (defaults to `std::io::stderr()`).
- A verbosity level (`Quiet`, `Normal`, `Verbose`).
- A colour mode (`Auto`, `Always`, `Never`), resolved against whether the sink is a TTY.

All subcommands receive `&mut Shell` as an explicit parameter (or via a context struct).
No `eprintln!` calls appear in library code; all diagnostic output goes through `Shell`
methods (`shell.status(...)`, `shell.warn(...)`, `shell.error(...)`).

`Shell` is injectable so tests can pass a `Vec<u8>` sink and assert on diagnostic text
without capturing the real stderr.

### Data output pipeline

Payload output (the bytes returned by `read`, the diff produced by `--diff`, etc.) is
written to a separate `Box<dyn Write>` that defaults to `std::io::stdout()`.  The
`--output-format` flag controls how those bytes are encoded before writing.  This writer
is also injectable for testing.

### MG-7 dependency

MG-7 (`--output-format` for `read`) must be implemented after DS-8 is resolved and
`Shell` is in place, so that the data output path is clean and injectable from the
start.  Mixing error messages into stdout before MG-7 lands would require a disruptive
refactor later.

---

## `tpu find` — Encoding-Aware Pattern Search

### Motivation and name choice

The subcommand is named `find` rather than `grep` because it goes beyond single-file
line search: it accepts glob paths, multiple patterns, and AND/OR matching semantics
that grep does not standardise.  The name also avoids confusion with the `regex` crate's
documented "Rust regex" syntax, which differs from POSIX ERE / PCRE in meaningful ways.

### CLI shape

```
SIMPLE:    tpu find <PATTERN> <PATH>
ADVANCED:  tpu find --pattern P1 --pattern P2 --path G1 --path G2 [flags]
```

Positional `PATTERN` and `PATH` are shorthands for the first `--pattern` and `--path`.
All flags can be used in either form.  Providing neither a positional nor a flag value
for patterns or paths is a hard CLI error (exit 2); there is no stdin fallback.

### Pattern syntax

Patterns use the `regex` crate, but only when opted into. By default every pattern
is a fixed literal string; pass `--regex` (short: `-E`) to interpret patterns using
regex syntax instead: <https://docs.rs/regex/latest/regex/#syntax>

Regex is opt-in (not the default) so that an accidental metacharacter in a literal
search target never silently changes what gets matched. Without `--regex`, every
metacharacter (`{`, `(`, `.`, `*`, `+`, `?`, …) is matched literally via
`regex::escape`.

### Multiple patterns — OR vs AND

Multiple `--pattern` values are **OR'd** by default (any pattern matching a line makes
it a hit), which is consistent with `grep -e`.  The `--all-match` flag switches to
**AND mode**: every pattern must match the line for it to be a hit.  `--invert`
negates the effective predicate in both modes:

| Mode | `--invert` absent | `--invert` present |
|---|---|---|
| OR (default) | line matches ≥1 pattern | line matches 0 patterns |
| `--all-match` | line matches all patterns | line fails ≥1 pattern |

### File selection — `--path` and globs

`--path` accepts either a bare file path or a glob pattern (e.g. `"src/**/*.rs"`).
Multiple `--path` values are allowed and are expanded independently; the union of all
expanded files is searched in the order they are produced by `walkdir`.  The `globset`
crate is used for glob pattern matching and `walkdir` for directory traversal.

Zero files matched by a glob is a hard error (exit 2) — it is almost always a mistake.

### Output format

| Situation | Format |
|---|---|
| Single path, no `--numbers` | `<text>` |
| Single path, `--numbers` | `<lineno>:<text>` |
| Multi-path, no `--numbers` | `<file>:<text>` |
| Multi-path, `--numbers` | `<file>:<lineno>:<text>` |
| Context lines | `<file>-<lineno>-<text>` (or `<lineno>-<text>` for single path) |
| Group separator (with `-A`/`-B`) | `--` |
| Count mode (per file) | `<file>: <N>` |
| Count mode (total) | `total: <N>` — only when >1 file matched |

JSON output (`--message-format=json`): one NDJSON data object per emitted entry with
fields `file`, `line_number`, `text`, `is_context`.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | ≥1 match found (or `--count` mode completed without error) |
| 1 | No matches |
| 2 | Error (bad glob, invalid regex, I/O failure, missing args) |

This matches the grep convention.

### Relation to `search.rs`

A substantial `search.rs` module already exists in `src/tools/tpu/src/cmd/` with a
working `run()` function (context lines, `--count`, a fixed-string matching option,
`--multiline`) but is dead code (not wired to any CLI command).  `tpu find` will
promote and extend this module rather than start from scratch: it will be
renamed/moved to `find.rs` and extended to support multiple patterns, multiple
files, and glob expansion.

---

## `tpu edit` — Targeted In-Place Edits

### Purpose

`tpu write` rewrites entire files and `tpu replace` applies regex patterns, but neither
supports edits at **known positions** (line numbers or byte offsets) without describing
the content.  `tpu edit` fills this gap.

### CLI shape

```
tpu edit <FILE> [--binary] [--data-format=hex|base64|encoded]
  [--delete RANGE]...           # remove byte/line range
  [--insert OFFSET DATA]...    # insert DATA before byte/line N
  [--splice RANGE DATA]...     # replace byte/line RANGE with DATA
  [--validate SELECTOR VALUE]... # same selectors as tpu write
  [--diff]
  [--line-ending lf|crlf|cr]  # line mode only; overrides detected ending
```

In **line mode** (default), `RANGE` and `OFFSET` are **1-based line numbers**, matching
`read --lines` and `--validate line:`.  In **binary mode** (`--binary`), they are
**0-based byte offsets**, matching `read --bytes` and `--validate bytes:`.

### `EditOp` enum

```rust
/// A single targeted edit operation.  All coordinates are expressed in the
/// coordinate space appropriate to the mode (source byte offsets for binary
/// mode; source byte offsets derived from line numbers for line mode).
/// **Changing the wire representation of this type is a breaking change.**
pub enum EditOp {
    /// Remove bytes in `[start, end)` from the source.  `end` is exclusive.
    Delete { start: usize, end: usize },
    /// Insert `data` immediately before byte offset `offset` in the source.
    /// Equivalent to `Splice { start: offset, end: offset, data }`.
    Insert { offset: usize, data: Vec<u8> },
    /// Replace bytes in `[start, end)` with `data`.  `end` is exclusive.
    /// `data` may be any length (shorter, longer, or the same).
    Splice { start: usize, end: usize, data: Vec<u8> },
}
```

All coordinates in `EditOp` are **source byte offsets** in the original file, regardless
of mode.  Line-mode callers must resolve line numbers to source byte ranges (via harrier)
before constructing `EditOp` values.

### `run()` function signature

```rust
/// Execute a set of edit operations on `file`.
///
/// # Coordinate model
///
/// - **Binary mode**: `ops` coordinates are 0-based byte offsets into the
///   original file content, identical to the `--bytes` selector space.
/// - **Line mode**: `ops` coordinates are source byte offsets derived from
///   1-based line numbers via `line_range_to_source_bytes`.  The caller
///   (not this function) performs the conversion so that the composability
///   invariant can be checked before any I/O occurs.
///
/// # Composability invariant
///
/// All `ops` coordinates MUST reference the **original file** as it exists
/// before this call.  `run` resolves them all at once before applying any
/// splice.  Ops are then applied in **reverse start-offset order** to a
/// forked redwing branch so that each lower-address op still sees its
/// original position undisturbed — the same strategy used by `tpu replace`.
///
/// Across successive `tpu edit` invocations the file is the already-written
/// result of the previous call (sequential composition for the caller;
/// original-coordinate composition within a single call).
///
/// # Overlapping-patch policy
///
/// Before any I/O, `run` checks that no two ops have overlapping `[start,
/// end)` ranges.  Any overlap is a hard error; the function returns an error
/// and the file is not modified.  Ops that are merely adjacent (end of one
/// == start of next) are permitted.
///
/// # Atomic write
///
/// On success, the result is written atomically:
/// temp file → rename original to `<file>.bak` → rename temp to original.
/// On failure after the `.bak` rename, the `.bak` is renamed back.
pub fn run(
    file: &Path,
    ops: Vec<EditOp>,
    binary: bool,
    line_ending_override: Option<LineEnding>,
    validate: &[(ValidateSelector, &str)],
    diff_out: Option<&mut dyn Write>,
) -> Result<usize, Box<dyn std::error::Error>>;
```

### Line-number → source-byte-range mapping

```rust
/// Convert a 1-based inclusive line range `[start_line, end_line]` to the
/// corresponding source byte range `[byte_start, byte_end)` using the harrier
/// `Lines` view.
///
/// `byte_start` is the first byte of `start_line`'s content in the original
/// source encoding.  `byte_end` is one past the last byte of `end_line`'s
/// line terminator (i.e. the entire terminator is included in the range, so
/// a delete of that range removes the terminator too).
///
/// Returns `Err` if either line number is 0 or greater than the number of
/// lines in the file.
pub fn line_range_to_source_bytes(
    source: &Source,
    start_line: usize,
    end_line: usize,
) -> Result<(usize, usize), Box<dyn std::error::Error>>;
```

The implementation uses `source.lines()` and `view.offset_map.to_source()`, the same
path used by `tpu replace` for regex match coordinate conversion.

### Interaction with `--validate`

`--validate` guards run **before** any edit is applied, using `cmd::validate::run_all`
(the same function used by `tpu write` and `tpu replace`).  Text selectors (`line:`,
`line-contains:`) are available in line mode; binary selectors (`bytes:`, `md5:`,
`crc32:`) are available in binary mode.  Any failed selector causes `run` to return an
error immediately, leaving the file unmodified.

### `--diff`

After a successful write, a unified text diff between the old and new normalised content
is written to `diff_out`.  In binary mode, a diff is not emitted (binary content has no
meaningful unified-diff representation); a JSON `status` message is emitted instead when
`--message-format=json`.

### tpu-mcp `edit_file` tool

The MCP tool schema mirrors the CLI:

```text
{
  "file": "string",
  "ops": [{ "op": "delete" | "insert" | "splice", "range": "N" | "N-M", "data": "string" (optional), "data_format": "hex" | "base64" | "encoded" (optional) }],
  "validate": [{ "selector": "string", "value": "string" }],
  "diff": bool (optional),
  "binary": bool (optional),
  "line_ending": "lf" | "crlf" | "cr" (optional)
}
```

The MCP server calls `tpu::cmd::edit::run()` directly with typed arguments.
Annotations: `readOnlyHint: false`, `destructiveHint: true`.

---

## tpu head — Head Subcommand

`tpu head` emits the first N lines or first N bytes of a file to stdout.

### `HeadMode` enum

```rust
pub enum HeadMode {
    /// Emit the first `n` logical lines.  Default n = 10.
    Lines { n: usize },
    /// Emit the first `n` raw bytes.
    Bytes { n: u64 },
}
```

`Lines` and `Bytes` are mutually exclusive; the CLI parser enforces this (attempting to
supply both `--lines` and `--bytes` in the same invocation is a fatal argument error).
`--binary` is only valid when `Bytes` mode is selected.

### `run()` signature

```rust
pub fn run(
    file: &std::path::Path,
    mode: HeadMode,
    out: &mut dyn std::io::Write,
) -> Result<(), Box<dyn std::error::Error>>
```

`out` is the destination writer; callers pass a handle to stdout.  The separation of the
writer from the file path makes the function unit-testable without touching real stdout.

### Output contract

**Line mode** — harrier detects the file encoding and native line ending.  The first `n`
logical lines (LF-normalised internally) are re-encoded using the file's detected ending
and written to `out` through the same `DenormaliseWriter` path used by `tpu read --lines`.
If the file contains fewer than `n` lines, all lines are emitted without error.  No
trailing newline is added beyond what the file already provides: if the last selected line
does not end with the file's line terminator, none is appended.

**Byte mode** — the raw byte stream is read via redwing without any encoding or
line-ending transformation.  The first `n` bytes are written verbatim to `out`.  If the
file is shorter than `n` bytes, all bytes are emitted without error.  `--binary` selects
this mode and suppresses any encoding-detection step.

---

## tpu tail — Tail Subcommand

`tpu tail` emits the last N lines or last N bytes of a file to stdout.

### `TailMode` enum

```rust
pub enum TailMode {
    /// Emit the last `n` logical lines.  Default n = 10.
    Lines { n: usize },
    /// Emit the last `n` raw bytes.
    Bytes { n: u64 },
}
```

`Lines` and `Bytes` are mutually exclusive; the CLI parser enforces this (attempting to
supply both `--lines` and `--bytes` in the same invocation is a fatal argument error).
`--binary` is only valid when `Bytes` mode is selected.

### `run()` signature

```rust
pub fn run(
    file: &std::path::Path,
    mode: TailMode,
    out: &mut dyn std::io::Write,
) -> Result<(), Box<dyn std::error::Error>>
```

`out` is the destination writer; callers pass a handle to stdout.  The separation of the
writer from the file path keeps the function unit-testable without touching real stdout.

### Ring-buffer strategy (line mode)

Line mode uses a **fixed-capacity ring buffer** of size `n` to identify the last `n`
lines without buffering the full file in memory.

Algorithm:

1. Decode the file via harrier (encoding detection, BOM skip, LF normalisation).
2. Split the normalised text on `'\n'` to obtain all logical lines.
3. Iterate over the lines, writing each into a `VecDeque<String>` whose capacity is
   capped at `n`.  When the deque is full the *oldest* entry is evicted from the front
   before the new entry is pushed to the back.
4. After iteration the deque contains the last `min(n, total_lines)` lines in order.
5. Re-encode and emit each line using the file's detected line ending (same
   `DenormaliseWriter` path used by `tpu read --lines`).

Because `split('\n')` materialises the normalised text anyway, the ring buffer mainly
limits peak *line storage* rather than total memory use.  For files whose normalised text
fits comfortably in memory this is acceptable; very large files should be handled by a
future streaming variant if needed.

### Output contract

**Line mode** — harrier detects the file encoding and native line ending.  The last `n`
logical lines are re-encoded using the file's detected ending and written to `out`.
If the file contains fewer than `n` lines, all lines are emitted without error.  No
trailing newline is added beyond what the file already provides.

**Byte mode** — the file length is determined, then `max(0, file_len − n)` bytes are
skipped and the remainder is written verbatim to `out` without any encoding or
line-ending transformation.  If the file is shorter than `n` bytes, all bytes are
emitted without error.  `--binary` selects this mode and suppresses any
encoding-detection step.
