// Copyright (c) 2026, Michael Grier

mod cmd;
mod data_format;
mod encoding;
mod escape;
mod message;
mod mojibake;
mod output;
mod rsp;
mod shell;

use std::{fs, io, path::PathBuf};

use clap::{Parser, Subcommand};
use data_format::DataFormat;
use encoding::{BomPolicy, OutputEncoding};
use shell::Shell;
use tpu::IoMode;
// Re-export library functions so that cmd modules (which are compiled in both
// the lib and bin crate contexts) can use `crate::open_as_branch` etc.
pub use tpu::{open_as_branch, read_raw_bytes, retry_io};

/// Text Processing Utility — encoding-aware file tools for command-line and
/// agent (Copilot) use.
///
/// All subcommands operate on the file's native encoding and line-ending
/// convention transparently.  Input and output at the terminal are always
/// UTF-8 with LF line endings so that calling agents never need to reason
/// about encoding or CRLF.
#[derive(Parser)]
#[command(name = "tpu", version)]
struct Cli {
    /// Output format for machine-readable consumers.
    ///
    ///   human — coloured, human-readable messages on stderr (default).
    ///
    ///   json  — newline-delimited JSON objects on stdout (NDJSON, one
    ///           object per line); stderr is silent.  Each object has a
    ///           `reason` field identifying its type.  Mirrors Cargo's
    ///           `--message-format=json` convention.
    #[arg(
        long,
        global = true,
        value_name = "FORMAT",
        default_value = "human",
        value_parser = ["human", "json"]
    )]
    message_format: String,

    /// Suppress the read-time mojibake advisory (Milestone 4).
    ///
    /// By default, when a `read`/`readex`/`head`/`tail` decodes a file
    /// whose decoded text appears to contain mojibake (e.g. `cafÃ©`),
    /// `tpu` emits a single `note: <path>: file appears to contain
    /// mojibake (...); run 'tpu doctor' for details` line to its
    /// diagnostics writer.  This flag (or the `TPU_NO_MOJIBAKE_WARNING`
    /// environment variable) suppresses the note.  Reads themselves are
    /// never blocked.
    #[arg(long, global = true)]
    no_mojibake_warning: bool,

    /// How to handle per-entry errors during a multi-file or recursive
    /// operation (`tpu find`, `tpu doctor`, `tpu copy --recursive`).
    ///
    ///   warn — emit a warning record (NDJSON
    ///          `{"reason":"warning"}` in JSON mode, a yellow `warning:`
    ///          line on stderr in human mode) and continue with the next
    ///          entry. This is the default — a single inaccessible
    ///          directory no longer aborts a large scan.
    ///
    ///   fail — restore the legacy "abort on first walk error" behaviour.
    ///
    /// Has no effect on operations that touch a single explicit file.
    #[arg(
        long,
        global = true,
        value_name = "MODE",
        default_value = "warn",
        value_parser = ["warn", "fail"]
    )]
    on_error: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Read a file, emitting its content as UTF-8 with LF line endings to stdout.
    ///
    /// Accepts any encoding detectable by harrier (UTF-8, UTF-16LE/BE,
    /// Windows-1252, Shift-JIS, …) and any line-ending convention (LF,
    /// CRLF, CR).  The output is always UTF-8 with LF terminators and no
    /// byte-order mark (BOM) by default.  Use --utf8 --bom=preserve or
    /// --utf8 --bom=force to include a UTF-8 BOM when required.
    Read {
        /// File to read.
        file: PathBuf,

        /// Line range to emit: N (single line) or N-M (inclusive, 1-based).
        /// Omit to emit the entire file.
        #[arg(long)]
        lines: Option<String>,

        /// Prefix each output line with its 1-based line number.
        #[arg(long, short = 'n')]
        numbers: bool,

        /// Explicitly request UTF-8 output encoding.  The text content of
        /// `read` is always decoded to UTF-8, so this flag's primary use is
        /// to enable BOM control via --bom.
        #[arg(long)]
        utf8: bool,

        /// Controls whether a UTF-8 byte-order mark (BOM, U+FEFF) is
        /// prepended to the output.  Only meaningful with --utf8;
        /// supplying --bom without --utf8 is an error.
        ///
        ///   strip    — no BOM in output (default).
        ///
        ///   preserve — prepend a UTF-8 BOM only if the source file
        ///              itself contained a BOM.
        ///
        ///   force    — always prepend a UTF-8 BOM regardless of the
        ///              source file.
        #[arg(long, requires = "utf8", value_name = "MODE")]
        bom: Option<BomPolicy>,

        /// Emit the file's raw bytes in the tpu binary escape codec format
        /// (printable ASCII is passed through; other bytes appear as `\xHH`).
        /// Mutually exclusive with --utf8, --bom, --lines, and --numbers.
        #[arg(long, short = 'b', conflicts_with_all = ["utf8", "bom", "lines", "numbers"])]
        binary: bool,

        /// Byte range to emit: N (single byte) or N-M (inclusive, 1-based).
        /// Requires --binary.
        #[arg(long, requires = "binary")]
        bytes: Option<String>,

        /// Re-encode the binary output in the specified format instead of
        /// writing raw bytes.  Requires --binary.
        ///
        ///   hex     — UU-UU-UU-... (uppercase pairs, `-` separator).
        ///   base64  — PEM-body base64 (RFC 4648, 64-char lines, CRLF terminated).
        ///   encoded — 7-bit clean: printable ASCII unchanged, others as `\xHH`.
        #[arg(long, requires = "binary", value_name = "FORMAT")]
        output_format: Option<DataFormat>,

        /// Compute an integrity hash over a byte range of the file and include
        /// it in the JSON output.  Requires --binary.  Repeatable.
        ///
        /// Syntax: `--hash <algo>:<start>-<end>`
        ///   algo  — `crc32` or `md5`.
        ///   start — 0-based decimal or `0x`-prefixed hex byte offset.
        ///   end   — 0-based decimal or `0x`-prefixed hex byte offset,
        ///           or `$` / `EOF` to mean end-of-file.
        ///
        /// In human mode the hash result is silently omitted; the flag is
        /// still accepted so `.rsp` files work regardless of output mode.
        #[arg(long, requires = "binary", value_name = "ALGO:RANGE", action = clap::ArgAction::Append)]
        hash: Vec<String>,
    },

    /// Write stdin (UTF-8/LF) to a file, preserving the file's encoding and
    /// line endings.
    ///
    /// If the file does not exist it is created as UTF-8/LF.  If it exists,
    /// its encoding and dominant line-ending convention are detected and the
    /// written bytes are re-encoded and denormalised to match.  The original
    /// file is renamed to <file>.bak before the new content is written.
    ///
    /// By default the output encoding matches the existing file.  Use --utf8
    /// to force UTF-8 output, and --bom to control whether a UTF-8 BOM is
    /// prepended (strip by default, preserve to match the source file, force
    /// to always include one).
    Write {
        /// File to write.
        file: PathBuf,

        /// Force UTF-8 output encoding regardless of the existing file's
        /// encoding.  Without this flag the file's original encoding is
        /// preserved.
        #[arg(long)]
        utf8: bool,

        /// Controls whether a UTF-8 byte-order mark (BOM, U+FEFF) is
        /// prepended to the written file.  Only meaningful with --utf8;
        /// supplying --bom without --utf8 is an error.
        ///
        ///   strip    — no BOM in output (default).
        ///
        ///   preserve — write a UTF-8 BOM only if the existing file
        ///              contained a BOM (or omit it for new files).
        ///
        ///   force    — always prepend a UTF-8 BOM.
        #[arg(long, requires = "utf8", value_name = "MODE")]
        bom: Option<BomPolicy>,

        /// Write raw bytes to FILE with no encoding or line-ending transformation.
        /// The encoded binary data is supplied as the DATA positional argument with
        /// --data-format specifying its encoding, OR as raw bytes on stdin when
        /// --data-format is omitted.  Mutually exclusive with --utf8 and --bom.
        #[arg(long, short = 'b', conflicts_with_all = ["utf8", "bom"])]
        binary: bool,

        /// Treat the DATA positional argument as encoded in the given format
        /// and write its decoded bytes to FILE.  Requires --binary.
        ///
        ///   hex     — uppercase or lowercase hex digits, with optional `-`
        ///             separators (e.g. `4D-5A-00-00` or `4D5A0000`).
        ///
        ///   base64  — standard base-64 (RFC 4648) with required `=` padding.
        ///
        ///   encoded — tpu escape codec (`\xHH`, `\uXXXX`, `\n`, etc.).
        #[arg(long, requires = "binary", value_name = "FORMAT")]
        data_format: Option<DataFormat>,

        /// Content to write.
        ///
        /// In text mode: the UTF-8/LF text to write to FILE.  When omitted,
        /// content is read from stdin (UTF-8/LF), which is how `tpu-mcp`
        /// (tpu_write_file) always invokes this subcommand.
        ///
        /// In binary mode (--binary): the encoded bytes to decode and write;
        /// always required when --binary is active.
        #[arg(allow_hyphen_values = true)]
        data: Option<String>,

        /// Expected byte count of the decoded DATA; the write is aborted with a
        /// non-zero exit if the actual decoded length does not match.  VALUE is
        /// a decimal integer or a `0x`-prefixed hex integer (e.g. `0x1A`).
        /// Requires --binary.
        #[arg(long, requires = "binary", value_name = "VALUE")]
        data_length: Option<String>,

        /// Pre-write guard: validate SELECTOR VALUE against the target file
        /// before writing.  Can be repeated for multiple checks.  All
        /// validations run before any write; any failure leaves the file
        /// unchanged.
        ///
        ///   line:N              — line N (1-based) must exactly equal VALUE
        ///   line-contains:N     — line N must contain VALUE as a substring
        ///   bytes:OFFSET-END    — byte range [OFFSET,END) equals VALUE (hex)
        ///   md5:OFFSET-END      — MD5 of [OFFSET,END) equals VALUE (32 hex)
        ///   crc32:OFFSET-END    — CRC32 of [OFFSET,END) equals VALUE (8 hex)
        ///
        /// Text selectors require text mode; binary selectors require --binary.
        /// OFFSET and END are decimal or 0x-prefixed hex integers.
        #[arg(long, num_args = 2, value_names = ["SELECTOR", "VALUE"], action = clap::ArgAction::Append)]
        validate: Vec<String>,

        /// Write a unified diff of the changes to stdout after the file is
        /// successfully updated.
        #[arg(long)]
        diff: bool,

        /// Override the output line ending.  Without this flag the file's
        /// dominant line-ending convention is preserved.  Conflicts with
        /// --binary (binary mode does no line-ending processing).
        #[arg(long, conflicts_with = "binary", value_name = "ENDING",
              value_parser = ["lf", "crlf", "cr"])]
        line_ending: Option<String>,

        /// Bypass the write-time mojibake guard.  Without this flag, writes
        /// that introduce new mojibake matches relative to the file's prior
        /// content (e.g. accidental `é` from a PowerShell round-trip) are
        /// rejected.  Use this only when you intend to write content that
        /// legitimately contains mojibake digraphs.
        #[arg(long)]
        allow_mojibake: bool,
    },

    /// In-place regex replace in a file.
    ///
    /// The pattern is applied to a normalised (LF-only) view of the file so
    /// that patterns never need to account for CRLF.  The replacement
    /// template supports capture-group references ($0, $1, $name).  Use $$
    /// for a literal $.  The result is written atomically and the original
    /// is renamed to <file>.bak.
    ///
    /// By default the replacement string is interpreted with C-style
    /// backslash escapes (`\n` → newline, `\t` → tab, `\r` → CR, `\\` →
    /// backslash, `\0` → NUL, `\xHH` → raw byte, `\uXXXX` / `\UXXXXXXXX` →
    /// Unicode scalar).  Pass `--literal-replacement` to disable escape
    /// decoding and use the replacement bytes verbatim.  Capture-group
    /// references (`$0`, `$1`, `$name`, `$$`) are processed in either mode.
    Replace {
        /// File to modify in place.
        file: PathBuf,

        /// Regex pattern (regex::bytes syntax, applied to LF-normalised view).
        #[arg(allow_hyphen_values = true)]
        pattern: String,

        /// Replacement template.  Capture groups: $0/$1/$name.  Literal $: $$.
        /// By default backslash escapes (`\n`, `\t`, `\r`, `\\`, `\0`, `\xHH`,
        /// `\uXXXX`, `\UXXXXXXXX`) are decoded into their corresponding
        /// bytes; pass `--literal-replacement` to disable.
        #[arg(allow_hyphen_values = true)]
        replacement: String,

        /// Treat `pattern` as a fixed literal string; disable all regex metacharacters.
        /// Use this when the search target contains characters such as `{`, `(`, `.`,
        /// `*`, or `+` that would otherwise be interpreted as regex syntax.
        #[arg(long, short = 'F')]
        fixed_strings: bool,

        /// Treat the replacement string as raw bytes — do not interpret
        /// backslash escapes such as `\n`, `\t`, `\\`, `\xHH`, `\uXXXX`.
        /// Without this flag the replacement is decoded using `tpu`'s
        /// standard escape codec so that, for example, `\n` produces a
        /// newline.  Capture-group references (`$1`, `$name`, `$$`) are
        /// processed regardless of this flag.
        #[arg(long, short = 'L')]
        literal_replacement: bool,

        /// Apply pattern as a multiline regex: `^` and `$` match at LF boundaries
        /// within the file rather than only at the start and end of the entire
        /// content.  `\n` in patterns always matches the LF used in the
        /// normalised view.
        #[arg(long, short = 'm')]
        multiline: bool,

        /// Write a unified text diff of the changes to stdout after the file is
        /// successfully updated.  Mutually exclusive with --count.
        #[arg(long, conflicts_with = "count")]
        diff: bool,

        /// Count the number of substitutions and print it to stdout.
        /// The file is not modified.  Mutually exclusive with --diff and --dry-run.
        #[arg(long, conflicts_with_all = ["diff", "dry_run"])]
        count: bool,

        /// Show a unified diff of what would change without modifying the file.
        /// Exits with code 1 if any substitution would be made, 0 if none.
        /// Mutually exclusive with --count.
        #[arg(long, conflicts_with = "count")]
        dry_run: bool,

        /// Override the output line ending.  Without this flag the file's
        /// dominant line-ending convention is preserved.
        #[arg(long, value_name = "ENDING", value_parser = ["lf", "crlf", "cr"])]
        line_ending: Option<String>,

        /// Bypass the write-time mojibake guard (see `tpu write
        /// --allow-mojibake`).
        #[arg(long)]
        allow_mojibake: bool,
    },

    /// Make targeted in-place edits at known positions (line numbers in text
    /// mode, byte offsets in binary mode) in a file.
    ///
    /// All RANGE and OFFSET values reference the **original file**.  Multiple
    /// patches in one invocation are resolved to original coordinates before
    /// any write and applied in reverse offset order so no patch shifts the
    /// position of another.
    Edit {
        /// File to edit.
        file: PathBuf,

        /// Operate in binary mode: RANGE and OFFSET are 0-based byte offsets.
        /// Without this flag, RANGE and OFFSET are 1-based line numbers.
        #[arg(long, short = 'b')]
        binary: bool,

        /// Decode DATA arguments for --insert and --splice using this format.
        /// Requires --binary.
        ///
        ///   hex     — uppercase or lowercase hex digits with optional `-` separators.
        ///   base64  — standard base-64 (RFC 4648) with required `=` padding.
        ///   encoded — tpu escape codec (`\xHH`, `\uXXXX`, `\n`, etc.).
        #[arg(long, requires = "binary", value_name = "FORMAT")]
        data_format: Option<DataFormat>,

        /// Delete the line or byte RANGE.  RANGE is N (single) or N-M
        /// (inclusive).  Use `$` or `EOF` for the last line / last byte.
        /// Repeatable; all positions reference the original file.
        #[arg(long, value_name = "RANGE", action = clap::ArgAction::Append)]
        delete: Vec<String>,

        /// Insert DATA immediately before line or byte OFFSET.  Use `$` or
        /// `EOF` to append after the last line / last byte.  Repeatable; all
        /// positions reference the original file.
        #[arg(long, num_args = 2, value_names = ["OFFSET", "DATA"], action = clap::ArgAction::Append)]
        insert: Vec<String>,

        /// Replace the line or byte RANGE with DATA.  RANGE is N (single) or
        /// N-M (inclusive).  Use `$` or `EOF` for the last line / last byte.
        /// Repeatable; all positions reference the original file.
        #[arg(long, num_args = 2, value_names = ["RANGE", "DATA"], action = clap::ArgAction::Append)]
        splice: Vec<String>,

        /// Pre-edit guard: validate SELECTOR VALUE against the target file
        /// before applying any edits.  Repeatable; all validations run before
        /// any edit; any failure leaves the file unchanged.
        ///
        ///   line:N              — line N (1-based) must exactly equal VALUE
        ///   line-contains:N     — line N must contain VALUE as a substring
        ///   bytes:OFFSET-END    — byte range [OFFSET,END) equals VALUE (hex)
        ///   md5:OFFSET-END      — MD5 of [OFFSET,END) equals VALUE (32 hex)
        ///   crc32:OFFSET-END    — CRC32 of [OFFSET,END) equals VALUE (8 hex)
        ///
        /// Text selectors require text mode; binary selectors require --binary.
        #[arg(long, num_args = 2, value_names = ["SELECTOR", "VALUE"], action = clap::ArgAction::Append)]
        validate: Vec<String>,

        /// Write a unified text diff of the changes to stdout after a
        /// successful edit.  Not available in binary mode.
        #[arg(long)]
        diff: bool,

        /// Override the output line ending in text mode.  Without this flag
        /// the file's dominant line-ending convention is preserved.
        /// Conflicts with --binary.
        #[arg(long, conflicts_with = "binary", value_name = "ENDING",
              value_parser = ["lf", "crlf", "cr"])]
        line_ending: Option<String>,

        /// Bypass the write-time mojibake guard (see `tpu write
        /// --allow-mojibake`).  No-op in --binary mode (binary edits are
        /// not text-checked).
        #[arg(long)]
        allow_mojibake: bool,
    },

    /// Read a file and emit its content as a single 7-bit clean ASCII line,
    /// with every non-printable character — including all line breaks —
    /// escaped using the readex codec.
    ///
    /// Output format: a single flat line (one actual newline at the very end).
    /// Source line breaks appear as `\n` escape sequences; other non-ASCII or
    /// non-printable characters appear as `\uXXXX` or `\UXXXXXXXX`.  This
    /// format is safe for shell variables, JSON string values, and agent/tool
    /// output where 8-bit bytes or literal newlines can be misinterpreted.
    ///
    /// Use --utf8 --bom=preserve/force if the consuming tool requires a UTF-8
    /// BOM at the start of the output (same semantics as `read --utf8 --bom`).
    Readex {
        /// File to read.
        file: PathBuf,

        /// Line range to include: N (single line) or N-M (inclusive, 1-based).
        /// Omit to include the entire file.
        #[arg(long)]
        lines: Option<String>,

        /// Prefix each source line's escaped content with its 1-based line
        /// number, separated by two spaces.
        #[arg(long, short = 'n')]
        numbers: bool,

        /// Explicitly request UTF-8-compatible output.  The escaped content is
        /// always 7-bit ASCII (and thus valid UTF-8), so this flag's primary
        /// use is to enable BOM control via --bom.
        #[arg(long)]
        utf8: bool,

        /// Controls whether a UTF-8 byte-order mark (BOM, U+FEFF) is
        /// prepended to the output before the escaped text.  Only meaningful
        /// with --utf8; supplying --bom without --utf8 is an error.
        ///
        ///   strip    — no BOM in output (default).
        ///
        ///   preserve — prepend a UTF-8 BOM only if the source file
        ///              itself contained a BOM.
        ///
        ///   force    — always prepend a UTF-8 BOM regardless of the
        ///              source file.
        #[arg(long, requires = "utf8", value_name = "MODE")]
        bom: Option<BomPolicy>,

        /// Emit the file's raw bytes in the tpu binary escape codec format
        /// (printable ASCII is passed through; other bytes appear as `\xHH`).
        /// Mutually exclusive with --utf8, --bom, --lines, and --numbers.
        #[arg(long, short = 'b', conflicts_with_all = ["utf8", "bom", "lines", "numbers"])]
        binary: bool,

        /// Byte range to emit: N (single byte) or N-M (inclusive, 1-based).
        /// Requires --binary.
        #[arg(long, requires = "binary")]
        bytes: Option<String>,

        /// Re-encode the binary output in the specified format instead of
        /// writing raw bytes.  Requires --binary.
        ///
        ///   hex     — UU-UU-UU-... (uppercase pairs, `-` separator).
        ///   base64  — PEM-body base64 (RFC 4648, 64-char lines, CRLF terminated).
        ///   encoded — 7-bit clean: printable ASCII unchanged, others as `\xHH`.
        #[arg(long, requires = "binary", value_name = "FORMAT")]
        output_format: Option<DataFormat>,
    },

    /// Emit the first N lines or N bytes of a file to stdout.
    ///
    /// By default the first 10 lines are emitted using the file's native
    /// encoding and line-ending convention.  Use --lines to select a different
    /// count, or --bytes to select raw bytes instead of lines.
    Head {
        /// File to read.
        file: PathBuf,

        /// Number of lines to emit (1-based first N lines).  Default: 10.
        /// Mutually exclusive with --bytes.
        #[arg(long, conflicts_with = "bytes")]
        lines: Option<usize>,

        /// Number of bytes to emit (first N raw bytes).  Mutually exclusive
        /// with --lines.
        #[arg(long, conflicts_with = "lines")]
        bytes: Option<u64>,

        /// Suppress encoding detection; treat the file as raw bytes.  Only
        /// valid with --bytes.
        #[arg(long, short = 'b', requires = "bytes")]
        binary: bool,

        /// Prefix each output line with its 1-based line number followed by a
        /// tab.  Mutually exclusive with --bytes and --binary.
        #[arg(long, short = 'n', conflicts_with_all = ["bytes", "binary"])]
        numbers: bool,
    },

    /// Emit the last N lines or N bytes of a file to stdout.
    ///
    /// By default the last 10 lines are emitted using the file's native
    /// encoding and line-ending convention.  Use --lines to select a different
    /// count, or --bytes to select raw bytes instead of lines.
    Tail {
        /// File to read.
        file: PathBuf,

        /// Number of lines to emit (last N lines).  Default: 10.
        /// Mutually exclusive with --bytes.
        #[arg(long, conflicts_with = "bytes")]
        lines: Option<usize>,

        /// Number of bytes to emit (last N raw bytes).  Mutually exclusive
        /// with --lines.
        #[arg(long, conflicts_with = "lines")]
        bytes: Option<u64>,

        /// Suppress encoding detection; treat the file as raw bytes.  Only
        /// valid with --bytes.
        #[arg(long, short = 'b', requires = "bytes")]
        binary: bool,

        /// Prefix each output line with its absolute 1-based line number
        /// followed by a tab.  Mutually exclusive with --bytes and --binary.
        #[arg(long, short = 'n', conflicts_with_all = ["bytes", "binary"])]
        numbers: bool,
    },

    /// Move a contiguous block of lines from SOURCE to DEST.
    ///
    /// The block begins at the first line matching --start-pattern (inclusive)
    /// and ends just before the first subsequent line matching --end-pattern
    /// (exclusive), or at EOF if --end-pattern is omitted.
    ///
    /// The block is removed from SOURCE and appended to DEST (which is
    /// created if absent).  If --dest-header is given, that line is prepended
    /// to the moved block in DEST.
    ///
    /// Both files must be valid UTF-8 text.  Line terminators are preserved
    /// verbatim in the moved content; any synthesised line (dest-header or
    /// trailing-newline separator) uses SOURCE's dominant line ending.
    ///
    /// JSON output: data message with {moved_lines, source_file, dest_file}.
    ///
    /// Exit codes: 0 on success, 2 on error.
    #[command(name = "move-block")]
    MoveBlock {
        /// File to move lines from.
        source: PathBuf,

        /// File to append lines to (created if absent).
        dest: PathBuf,

        /// Rust regex marking the first line of the block (inclusive).
        #[arg(long)]
        start_pattern: String,

        /// Rust regex marking the end of the block (exclusive).
        /// Omit to extend the block to EOF.
        #[arg(long)]
        end_pattern: Option<String>,

        /// Line to prepend to the moved block in DEST (e.g. a section header).
        #[arg(long)]
        dest_header: Option<String>,
    },

    /// Append content to an existing file, preserving its encoding and line endings.
    ///
    /// The file's native encoding (UTF-8, UTF-16LE/BE, Windows-1252, …) and
    /// dominant line-ending convention are detected and the new content is
    /// re-encoded to match before being appended atomically.  The original file
    /// is renamed to <file>.bak before the new content is written.
    ///
    /// When --data is omitted, content is read from stdin (UTF-8/LF).
    Append {
        /// File to append to (must already exist; use 'write' to create new files).
        file: PathBuf,

        /// Text content to append (UTF-8, LF line endings).  When omitted,
        /// content is read from stdin.
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        data: Option<String>,

        /// Pre-append guard: validate SELECTOR VALUE against the target file
        /// before appending.  Can be repeated for multiple checks.  All
        /// validations run before any write; any failure leaves the file
        /// unchanged.
        ///
        ///   line:N              — line N (1-based) must exactly equal VALUE
        ///   line-contains:N     — line N must contain VALUE as a substring
        ///   bytes:OFFSET-END    — byte range [OFFSET,END) equals VALUE (hex)
        ///   md5:OFFSET-END      — MD5 of [OFFSET,END) equals VALUE (32 hex)
        ///   crc32:OFFSET-END    — CRC32 of [OFFSET,END) equals VALUE (8 hex)
        #[arg(long, num_args = 2, value_names = ["SELECTOR", "VALUE"], action = clap::ArgAction::Append)]
        validate: Vec<String>,

        /// Show a unified diff of what would be appended, but do not modify
        /// the file (dry-run / preview mode).
        #[arg(long)]
        diff: bool,

        /// Override the line ending used for the appended content and the
        /// re-encoded combined file.  Without this flag the file's dominant
        /// line-ending convention is used.
        #[arg(long, value_name = "ENDING", value_parser = ["lf", "crlf", "cr"])]
        line_ending: Option<String>,

        /// Bypass the write-time mojibake guard (see `tpu write
        /// --allow-mojibake`).
        #[arg(long)]
        allow_mojibake: bool,
    },

    /// Count lines, words, characters, bytes, or regex pattern occurrences in a file.
    ///
    /// When no metric flag is given, all four standard metrics (lines, words,
    /// chars, bytes) are reported.  Each --pattern adds an additional named
    /// count.  Results are emitted one per line in declaration order.
    ///
    /// Human output format (one entry per line):
    ///   <label>: <count>
    ///
    /// JSON output: data message per entry, or a single array when
    ///   --message-format=json is active (via the global flag).
    ///
    /// Exit codes: 0 on success, non-zero on error.
    Count {
        /// File to inspect.
        file: PathBuf,

        /// Count logical lines (newline-delimited, encoding-aware).
        #[arg(long, short = 'l')]
        lines: bool,

        /// Count words (whitespace-delimited tokens in the decoded text).
        #[arg(long, short = 'w')]
        words: bool,

        /// Count Unicode scalar values (chars) in the decoded text.
        #[arg(long, short = 'c', visible_alias = "chars")]
        chars: bool,

        /// Count raw bytes (file size on disk).
        #[arg(long, short = 'b')]
        bytes: bool,

        /// Count non-overlapping occurrences of PATTERN (Rust regex) in the
        /// decoded text.  Repeatable; each instance adds one count entry.
        #[arg(long, value_name = "PATTERN", action = clap::ArgAction::Append)]
        pattern: Vec<String>,

        /// Human-readable label for the corresponding --pattern entry
        /// (positionally aligned).  Any label supplied beyond the number of
        /// patterns is an error; missing labels default to the pattern string.
        #[arg(long, value_name = "LABEL", action = clap::ArgAction::Append)]
        label: Vec<String>,

        /// Emit file metadata (encoding name, BOM presence, line-ending style)
        /// before the metric counts.  When combined with no other flags the
        /// default metric set (lines, words, chars, bytes) is still emitted.
        /// Stats are always included in JSON output regardless of this flag.
        #[arg(long)]
        stats: bool,
    },

    /// Search for a pattern in one or more files (encoding-aware).
    ///
    /// Reads each file through harrier so that UTF-8, UTF-16LE/BE,
    /// Windows-1252, and other encodings are transparently decoded.  Matched
    /// lines are emitted as UTF-8/LF to stdout.
    ///
    /// Simple:   `tpu find <PATTERN> <PATH>`
    /// Advanced: `tpu find --pattern P1 --pattern P2 --path G1 --path G2 [flags]`
    ///
    /// The positional PATTERN and PATH are shorthands for the first --pattern
    /// and --path respectively.  At least one pattern and one path are
    /// required; there is no stdin fallback.
    Find {
        /// Pattern to search for (positional shorthand for the first --pattern).
        pattern: Option<String>,

        /// File or glob to search in (positional shorthand for the first --path).
        path: Option<String>,

        /// Pattern(s) to search for (Rust regex syntax).  Repeatable.
        /// Multiple patterns are OR'd by default; use --all-match for AND.
        #[arg(long = "pattern", value_name = "PATTERN", action = clap::ArgAction::Append)]
        patterns: Vec<String>,

        /// Path(s) or glob(s) to search in.  Repeatable.
        /// A glob is any path containing `*`, `?`, `[`, or `{`.
        #[arg(long = "path", value_name = "GLOB", action = clap::ArgAction::Append)]
        paths: Vec<String>,

        /// In multi-pattern mode, require ALL patterns to match (AND).
        /// By default any single pattern match is sufficient (OR).
        #[arg(long)]
        all_match: bool,

        /// Treat each pattern as a fixed string; disable regex metacharacters.
        #[arg(long, short = 'F')]
        fixed_strings: bool,

        /// Match case-insensitively (equivalent to prefixing every pattern
        /// with `(?i)`).
        #[arg(long, short = 'i')]
        ignore_case: bool,

        /// Prefix each matched line with its 1-based line number.
        #[arg(long, short = 'n')]
        numbers: bool,

        /// Instead of matched lines, print the count of matching lines per file.
        /// With multiple files a `total: N` line is also emitted.
        #[arg(long, short = 'c')]
        count: bool,

        /// Invert the match: emit lines that do NOT match the predicate.
        /// With --all-match: emit lines that fail at least one pattern.
        #[arg(long, short = 'v')]
        invert: bool,

        /// Make `^` and `$` match at LF boundaries within each line rather
        /// than only at start/end of the whole decoded line.
        #[arg(long, short = 'm')]
        multiline: bool,

        /// Emit this many lines of context AFTER each matching line.
        #[arg(long, short = 'A', value_name = "N", default_value = "0")]
        after: usize,

        /// Emit this many lines of context BEFORE each matching line.
        #[arg(long, short = 'B', value_name = "N", default_value = "0")]
        before: usize,
    },

    /// Diagnose encoding / mojibake corruption across one or more paths.
    ///
    /// Walks each path (file, directory, or shell-style glob) and reports
    /// every text file that either contains characteristic mojibake
    /// digraphs (`Ã©`, `â€"`, `â\"€`, `Â<NBSP>`) or whose bytes are
    /// invalid in the file's detected encoding (UTF-8, UTF-16, …).
    /// Files containing the `encoding-check: allow-mojibake` opt-out
    /// marker are reported as clean.
    ///
    /// With `--fix=peel`, each flagged file is reverse-decoded one
    /// layer; if the result has strictly fewer mojibake matches it is
    /// rewritten in place via the standard atomic-write path
    /// (a `.bak` is kept and the M2 write-time guard applies).
    ///
    /// Skips `.git/`, `node_modules/`, `target/`, and known-binary
    /// extensions.  Honours a top-level `.gitignore` if present (basic
    /// non-negation patterns only).
    ///
    /// Exit code is 0 only when zero issues remain after any requested
    /// fixes have been applied.
    Doctor {
        /// File(s), directory(ies), or glob(s) to scan.  Defaults to `.`
        /// (the current directory) when omitted.
        paths: Vec<String>,

        /// Output format.
        ///
        ///   human — per-file lines plus a one-line summary (default).
        ///
        ///   json  — a single pretty-printed JSON document with the
        ///           documented schema (see `cmd::doctor` module docs).
        #[arg(
            long,
            value_name = "FORMAT",
            default_value = "human",
            value_parser = ["human", "json"]
        )]
        format: String,

        /// Repair mode.
        ///
        ///   peel — for each flagged file, apply
        ///          `mojibake::looks_like_one_layer_peel` and rewrite
        ///          the file in place when the result is strictly
        ///          better.  Without this flag no file is modified.
        #[arg(long, value_name = "MODE", value_parser = ["peel"])]
        fix: Option<String>,

        /// Suppress per-file lines in human mode; the summary is still
        /// printed.  No effect on JSON mode.
        #[arg(long, short = 'q')]
        quiet: bool,
    },

    /// Copy a file or recursively copy a directory tree.
    ///
    /// Bytes are copied verbatim — no encoding or line-ending transformation
    /// is applied. When SRC contains a glob meta-character (`*`, `?`, `[`,
    /// `{`) the matches are copied flat into DEST (which must be a
    /// directory). For a directory copy, pass `--recursive`.
    ///
    /// Per-entry errors honour the global `--on-error` flag: by default
    /// they produce a warning record and the copy continues with
    /// the remaining entries.
    Copy {
        /// Source file, directory, or glob pattern.
        source: String,

        /// Destination file or directory.
        dest: PathBuf,

        /// Recurse into directories. Required when SRC is a directory.
        #[arg(long, short = 'r')]
        recursive: bool,

        /// Overwrite existing destination files. Without this flag an
        /// existing target is skipped (and counted in the report).
        #[arg(long)]
        overwrite: bool,
    },

    /// Render a file from a `{{TOKEN}}`-style template.
    ///
    /// Tokens are written `{{NAME}}` (whitespace inside the braces is
    /// tolerated) and substituted with values supplied by repeatable
    /// `--var KEY=VALUE` pairs. Use `\{{` to emit literal braces.
    ///
    /// The template source is one of `--template <STRING>`,
    /// `--template-file <PATH>`, or stdin (when neither flag is given).
    /// The rendered text is written through `tpu`'s normal write path so
    /// the destination receives the standard mojibake guard, atomic .bak
    /// handling, and encoding preservation.
    Render {
        /// Output file to populate with the rendered template.
        output: PathBuf,

        /// Inline template string. Mutually exclusive with `--template-file`
        /// and stdin.
        #[arg(long, value_name = "STRING", conflicts_with = "template_file")]
        template: Option<String>,

        /// Path to a template file (decoded with the same rules as
        /// `tpu read`). Mutually exclusive with `--template` and stdin.
        #[arg(long = "template-file", value_name = "PATH")]
        template_file: Option<PathBuf>,

        /// Repeatable `KEY=VALUE` pair. Keys must match `[A-Za-z0-9_-]+`.
        #[arg(long = "var", value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
        var: Vec<String>,

        /// What to do when the template references a token absent from
        /// the supplied vars: `error` (default), `empty`, or `leave`.
        #[arg(
            long,
            value_name = "POLICY",
            default_value = "error",
            value_parser = ["error", "empty", "leave"]
        )]
        missing: String,

        /// Disable the write-time mojibake guard for the output file.
        #[arg(long)]
        allow_mojibake: bool,
    },

    /// Emit (or inject) the canonical Copilot-instructions guidance block
    /// for the `tpu-mcp` server's tools.
    ///
    /// Without `--inject` the block is printed to stdout. With `--inject`
    /// the block is idempotently merged into the named file: an existing
    /// managed block (delimited by `<!-- tpu-mcp:setup:begin -->` /
    /// `<!-- tpu-mcp:setup:end -->`) is replaced; otherwise the block is
    /// appended after a single blank line.
    Setup {
        /// File to update in place. When omitted, the block is printed to
        /// stdout. Typical value: `.github/copilot-instructions.md`.
        #[arg(long, value_name = "PATH")]
        inject: Option<PathBuf>,
    },
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let cli = if let Some(rsp_path) = rsp::try_rsp_path(&argv) {
        // @file invocation: read and tokenise the response file, then
        // re-parse the expanded argv through the normal clap machinery.
        let content = match std::fs::read_to_string(rsp_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: response file {rsp_path:?}: {e}");
                std::process::exit(1);
            }
        };
        let tokens = match rsp::tokenize_rsp(&content) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        };
        let mut new_argv: Vec<String> = Vec::with_capacity(1 + tokens.len());
        new_argv.push(argv[0].clone());
        new_argv.extend(tokens);
        Cli::try_parse_from(new_argv).unwrap_or_else(|e| e.exit())
    } else {
        Cli::parse()
    };
    let mut shell = if cli.message_format == "json" {
        Shell::new_json()
    } else {
        Shell::new()
    };
    let mut out: Box<dyn output::Output> = if cli.message_format == "json" {
        output::json_output()
    } else {
        output::human_output()
    };
    match run(cli, &mut shell, out.as_mut()) {
        Ok(()) => {
            let _ = shell.emit_finished(true);
        }
        Err(e) => {
            let _ = shell.error(e);
            let _ = shell.emit_finished(false);
            std::process::exit(1);
        }
    }
}

fn run(
    cli: Cli,
    shell: &mut Shell,
    out: &mut dyn output::Output,
) -> Result<(), Box<dyn std::error::Error>> {
    let json_mode = cli.message_format == "json";
    // Read-time mojibake advisory (Milestone 4).  Enabled by default;
    // disabled by `--no-mojibake-warning` or `TPU_NO_MOJIBAKE_WARNING=1`,
    // or whenever we are emitting JSON (where stderr is silent and we
    // would otherwise corrupt the NDJSON stream on stdout).
    let mojibake_advisory_enabled = !cli.no_mojibake_warning
        && std::env::var_os("TPU_NO_MOJIBAKE_WARNING").is_none()
        && !json_mode;
    let on_error_mode = match cli.on_error.as_str() {
        "fail" => cmd::copy::OnError::Fail,
        _ => cmd::copy::OnError::Warn,
    };
    match cli.command {
        Commands::Read {
            file,
            lines,
            numbers,
            utf8,
            bom,
            binary,
            bytes,
            output_format,
            hash,
        } => {
            if binary {
                let byte_range = match bytes.as_deref() {
                    None => None,
                    Some(s) => Some(cmd::read::parse_bytes_arg(s)?),
                };
                // Parse all --hash args eagerly so bad syntax is caught before any I/O.
                let hash_specs: Vec<cmd::read::HashSpec> = hash
                    .iter()
                    .map(|s| cmd::read::parse_hash_arg(s))
                    .collect::<Result<Vec<_>, _>>()?;
                let all_bytes = fs::read(&file)?;
                // Compute hashes before emitting output; errors abort with no output.
                let computed_hashes = cmd::read::compute_hashes(&all_bytes, &hash_specs)?;
                // Build the JSON hashes array from computed results.
                let hashes_json = serde_json::Value::Array(
                    computed_hashes
                        .iter()
                        .map(|h| {
                            serde_json::json!({
                                "algo": match h.algo {
                                    cmd::read::HashAlgo::Crc32 => "crc32",
                                    cmd::read::HashAlgo::Md5 => "md5",
                                },
                                "range": format!("{}-{}", h.start, h.resolved_end),
                                "value": h.hex_value,
                            })
                        })
                        .collect(),
                );
                let slice: &[u8] = match byte_range {
                    None => &all_bytes[..],
                    Some((lo, hi)) => {
                        let lo = (lo.saturating_sub(1) as usize).min(all_bytes.len());
                        let hi = (hi as usize).min(all_bytes.len());
                        &all_bytes[lo..hi]
                    }
                };
                if let Some(ref fmt) = output_format {
                    let encoded = data_format::encode(fmt, slice);
                    out.emit("read", None, None, format_args!("{}", encoded));
                    return Ok(());
                }
                out.emit_binary_hashed("read", None, None, slice, &hashes_json);
                Ok(())
            } else {
                let lines_range = match lines.as_deref() {
                    None => None,
                    Some(s) => Some(cmd::read::parse_lines_arg(s)?),
                };
                let output_encoding = if utf8 {
                    OutputEncoding::Utf8
                } else {
                    OutputEncoding::Preserve
                };
                let bom_policy = bom.unwrap_or_default();
                let mut buf: Vec<u8> = Vec::new();
                let mut notes_buf: Vec<u8> = Vec::new();
                let notes: Option<&mut dyn std::io::Write> = if mojibake_advisory_enabled {
                    Some(&mut notes_buf)
                } else {
                    None
                };
                cmd::read::run(
                    &file,
                    lines_range,
                    numbers,
                    output_encoding,
                    bom_policy,
                    &mut buf,
                    IoMode::Mmap,
                    notes,
                )?;
                if !notes_buf.is_empty() {
                    let _ = shell.err().write_all(&notes_buf);
                }
                let content =
                    String::from_utf8(buf).map_err(|e| format!("read: non-UTF-8 output: {e}"))?;
                out.emit("read", None, None, format_args!("{}", content));
                Ok(())
            }
        }

        Commands::Write {
            file,
            utf8,
            bom,
            binary,
            data_format,
            data,
            data_length,
            validate,
            diff,
            line_ending,
            allow_mojibake,
        } => {
            cmd::validate::run_all(&validate, &file, binary, IoMode::Mmap)?;
            let mut diff_buf: Vec<u8> = Vec::new();
            let diff_out: Option<&mut dyn io::Write> =
                if diff { Some(&mut diff_buf) } else { None };

            let result = if binary {
                // Binary mode: two sub-modes.
                //   --data-format present: decode the DATA positional argument.
                //   --data-format absent, no DATA positional: read raw bytes from stdin.
                //   --data-format absent, DATA positional present: error (ambiguous).
                let bytes = match (data_format.as_ref(), data) {
                    (Some(fmt), Some(raw)) => data_format::decode(fmt, &raw)
                        .map_err(|e| format!("--data-format decode error: {e}"))?,
                    (Some(_), None) => {
                        return Err(
                            "write --binary --data-format requires a DATA positional argument"
                                .into(),
                        );
                    }
                    (None, Some(_)) => {
                        return Err("write: DATA positional argument requires --data-format; \
                             omit DATA to read raw bytes from stdin"
                            .into());
                    }
                    (None, None) => {
                        use std::io::Read;
                        let mut buf = Vec::new();
                        io::stdin()
                            .read_to_end(&mut buf)
                            .map_err(|e| format!("write --binary: reading stdin: {e}"))?;
                        buf
                    }
                };
                if let Some(ref len_str) = data_length {
                    let expected = parse_data_length(len_str)?;
                    if bytes.len() != expected {
                        return Err(format!(
                            "--data-length mismatch: expected {expected} bytes, got {}",
                            bytes.len()
                        )
                        .into());
                    }
                }
                cmd::write::run_binary(&file, &bytes, diff_out)
            } else {
                // Accept content from the positional arg or from stdin.
                // tpu-mcp always passes content via stdin (no positional arg).
                let text = match data {
                    Some(s) => s,
                    None => {
                        use std::io::Read;
                        let mut s = String::new();
                        io::stdin()
                            .read_to_string(&mut s)
                            .map_err(|e| format!("write: reading stdin: {e}"))?;
                        s
                    }
                };
                let output_encoding = if utf8 {
                    OutputEncoding::Utf8
                } else {
                    OutputEncoding::Preserve
                };
                let bom_policy = bom.unwrap_or_default();
                let le_override = match line_ending.as_deref() {
                    None => None,
                    Some("lf") => Some(harrier::encoding::LineEnding::Lf),
                    Some("crlf") => Some(harrier::encoding::LineEnding::CrLf),
                    Some("cr") => Some(harrier::encoding::LineEnding::Cr),
                    Some(other) => {
                        return Err(format!(
                            "--line-ending: unrecognised value {other:?}; expected lf, crlf, or cr"
                        )
                        .into())
                    }
                };
                cmd::write::run(
                    &file,
                    &text,
                    output_encoding,
                    bom_policy,
                    le_override,
                    diff_out,
                    IoMode::Mmap,
                    if allow_mojibake {
                        mojibake::WritePolicy::permissive()
                    } else {
                        mojibake::WritePolicy::default()
                    },
                )
            };
            result?;
            if diff && !diff_buf.is_empty() {
                let content = String::from_utf8_lossy(&diff_buf).into_owned();
                out.emit_json(
                    "write",
                    None,
                    None,
                    &serde_json::json!({
                        "reason": "diff",
                        "subcommand": "write",
                        "content": content,
                        "rendered": content,
                    }),
                );
            }
            Ok(())
        }

        Commands::Replace {
            file,
            pattern,
            replacement,
            fixed_strings,
            literal_replacement,
            multiline,
            diff,
            count,
            dry_run,
            line_ending,
            allow_mojibake,
        } => {
            let mut diff_buf: Vec<u8> = Vec::new();
            let diff_out: Option<&mut dyn io::Write> = if diff || dry_run {
                Some(&mut diff_buf)
            } else {
                None
            };
            let le_override = match line_ending.as_deref() {
                None => None,
                Some("lf") => Some(harrier::encoding::LineEnding::Lf),
                Some("crlf") => Some(harrier::encoding::LineEnding::CrLf),
                Some("cr") => Some(harrier::encoding::LineEnding::Cr),
                Some(other) => {
                    return Err(format!(
                        "--line-ending: unrecognised value {other:?}; expected lf, crlf, or cr"
                    )
                    .into())
                }
            };
            // Decode backslash escapes in the replacement string unless the
            // caller asked for raw/literal handling.  This is the default
            // because users typically write `\n` expecting a real newline.
            let decoded_replacement = cmd::replace::decode_replacement(
                &replacement,
                literal_replacement,
            )
            .map_err(|e| format!("replace: {e}"))?;
            let n = cmd::replace::run(
                &file,
                &pattern,
                &decoded_replacement,
                multiline,
                fixed_strings,
                le_override,
                diff_out,
                count,
                dry_run,
                IoMode::Mmap,
                if allow_mojibake {
                    mojibake::WritePolicy::permissive()
                } else {
                    mojibake::WritePolicy::default()
                },
            )?;
            if count {
                out.emit_json(
                    "replace",
                    None,
                    None,
                    &serde_json::json!({
                        "reason": "count",
                        "subcommand": "replace",
                        "count": n,
                        "rendered": format!("{n}\n"),
                    }),
                );
                return Ok(());
            }
            if dry_run {
                if !diff_buf.is_empty() {
                    let content = String::from_utf8_lossy(&diff_buf).into_owned();
                    out.emit_json(
                        "replace",
                        None,
                        None,
                        &serde_json::json!({
                            "reason": "diff",
                            "subcommand": "replace",
                            "content": content,
                            "rendered": content,
                        }),
                    );
                }
                if n > 0 {
                    std::process::exit(1);
                }
                return Ok(());
            }
            if diff && !diff_buf.is_empty() {
                let content = String::from_utf8_lossy(&diff_buf).into_owned();
                out.emit_json(
                    "replace",
                    None,
                    None,
                    &serde_json::json!({
                        "reason": "diff",
                        "subcommand": "replace",
                        "content": content,
                        "rendered": content,
                    }),
                );
            }
            shell.status(
                "replace",
                format!(
                    "{}: {} replacement{}",
                    file.display(),
                    n,
                    if n == 1 { "" } else { "s" }
                ),
            )?;
            Ok(())
        }

        Commands::Edit {
            file,
            binary,
            data_format,
            delete,
            insert,
            splice,
            validate,
            diff,
            line_ending,
            allow_mojibake,
        } => {
            let line_ending_override = match line_ending.as_deref() {
                None => None,
                Some("lf") => Some(harrier::encoding::LineEnding::Lf),
                Some("crlf") => Some(harrier::encoding::LineEnding::CrLf),
                Some("cr") => Some(harrier::encoding::LineEnding::Cr),
                Some(other) => {
                    return Err(format!(
                        "--line-ending: unrecognised value {other:?}; expected lf, crlf, or cr"
                    )
                    .into())
                }
            };
            cmd::validate::run_all(&validate, &file, binary, IoMode::Mmap)?;

            // Parse all ops before any I/O so bad-syntax errors are caught
            // up front and the file is guaranteed unmodified on any error.
            let mut ops: Vec<cmd::edit::EditOp> = Vec::new();

            for range_s in &delete {
                let (start, end) = if binary {
                    cmd::edit::parse_byte_range(range_s).map_err(|e| format!("--delete: {e}"))?
                } else {
                    cmd::edit::parse_line_range(range_s).map_err(|e| format!("--delete: {e}"))?
                };
                ops.push(cmd::edit::EditOp::Delete { start, end });
            }

            for chunk in insert.chunks(2) {
                let offset_s = &chunk[0];
                let data_s = &chunk[1];
                let offset = if binary {
                    cmd::edit::parse_byte_pos(offset_s)
                        .map_err(|e| format!("--insert offset: {e}"))?
                } else {
                    cmd::edit::parse_line_num(offset_s)
                        .map_err(|e| format!("--insert offset: {e}"))?
                };
                let data = if let Some(ref fmt) = data_format {
                    data_format::decode(fmt, data_s)
                        .map_err(|e| format!("--insert --data-format decode error: {e}"))?
                } else {
                    data_s.as_bytes().to_vec()
                };
                ops.push(cmd::edit::EditOp::Insert { offset, data });
            }

            for chunk in splice.chunks(2) {
                let range_s = &chunk[0];
                let data_s = &chunk[1];
                let (start, end) = if binary {
                    cmd::edit::parse_byte_range(range_s).map_err(|e| format!("--splice: {e}"))?
                } else {
                    cmd::edit::parse_line_range(range_s).map_err(|e| format!("--splice: {e}"))?
                };
                let data = if let Some(ref fmt) = data_format {
                    data_format::decode(fmt, data_s)
                        .map_err(|e| format!("--splice --data-format decode error: {e}"))?
                } else {
                    data_s.as_bytes().to_vec()
                };
                ops.push(cmd::edit::EditOp::Splice { start, end, data });
            }

            let mut diff_buf: Vec<u8> = Vec::new();
            let diff_out: Option<&mut dyn io::Write> = if diff && !binary {
                Some(&mut diff_buf)
            } else {
                None
            };
            let n = cmd::edit::run(
                &file,
                ops,
                binary,
                line_ending_override,
                diff_out,
                IoMode::Mmap,
                if allow_mojibake {
                    mojibake::WritePolicy::permissive()
                } else {
                    mojibake::WritePolicy::default()
                },
            )?;
            if diff && !diff_buf.is_empty() {
                let content = String::from_utf8_lossy(&diff_buf).into_owned();
                out.emit_json(
                    "edit",
                    None,
                    None,
                    &serde_json::json!({
                        "reason": "diff",
                        "subcommand": "edit",
                        "content": content,
                        "rendered": content,
                    }),
                );
            }
            shell.status(
                "edit",
                format!(
                    "{}: {} op{} applied",
                    file.display(),
                    n,
                    if n == 1 { "" } else { "s" }
                ),
            )?;
            Ok(())
        }

        Commands::Tail {
            file,
            lines,
            bytes,
            binary,
            numbers,
        } => {
            let mode = if let Some(n) = bytes {
                cmd::tail::TailMode::Bytes { n }
            } else if binary {
                cmd::tail::TailMode::Bytes { n: u64::MAX }
            } else {
                cmd::tail::TailMode::Lines {
                    n: lines.unwrap_or(10),
                    numbers,
                }
            };
            match mode {
                cmd::tail::TailMode::Lines { .. } => {
                    let mut buf: Vec<u8> = Vec::new();
                    let mut notes_buf: Vec<u8> = Vec::new();
                    let notes: Option<&mut dyn std::io::Write> = if mojibake_advisory_enabled {
                        Some(&mut notes_buf)
                    } else {
                        None
                    };
                    cmd::tail::run(&file, mode, &mut buf, IoMode::Mmap, notes)?;
                    if !notes_buf.is_empty() {
                        let _ = shell.err().write_all(&notes_buf);
                    }
                    let content = String::from_utf8(buf)
                        .map_err(|e| format!("tail: non-UTF-8 output: {e}"))?;
                    out.emit("tail", None, None, format_args!("{content}"));
                }
                cmd::tail::TailMode::Bytes { .. } => {
                    let mut stdout = std::io::stdout();
                    cmd::tail::run(&file, mode, &mut stdout, IoMode::Mmap, None)?;
                }
            }
            Ok(())
        }

        Commands::Head {
            file,
            lines,
            bytes,
            binary,
            numbers,
        } => {
            let mode = if let Some(n) = bytes {
                cmd::head::HeadMode::Bytes { n }
            } else if binary {
                cmd::head::HeadMode::Bytes { n: u64::MAX }
            } else {
                cmd::head::HeadMode::Lines {
                    n: lines.unwrap_or(10),
                    numbers,
                }
            };
            match mode {
                cmd::head::HeadMode::Lines { .. } => {
                    let mut buf: Vec<u8> = Vec::new();
                    let mut notes_buf: Vec<u8> = Vec::new();
                    let notes: Option<&mut dyn std::io::Write> = if mojibake_advisory_enabled {
                        Some(&mut notes_buf)
                    } else {
                        None
                    };
                    cmd::head::run(&file, mode, &mut buf, IoMode::Mmap, notes)?;
                    if !notes_buf.is_empty() {
                        let _ = shell.err().write_all(&notes_buf);
                    }
                    let content = String::from_utf8(buf)
                        .map_err(|e| format!("head: non-UTF-8 output: {e}"))?;
                    out.emit("head", None, None, format_args!("{content}"));
                }
                cmd::head::HeadMode::Bytes { .. } => {
                    // Write raw bytes directly to stdout; the Output abstraction
                    // would escape them (7-bit safety for readex), which is
                    // wrong for head --bytes / head --binary.
                    let mut stdout = std::io::stdout();
                    cmd::head::run(&file, mode, &mut stdout, IoMode::Mmap, None)?;
                }
            }
            Ok(())
        }

        Commands::Readex {
            file,
            lines,
            numbers,
            utf8,
            bom,
            binary,
            bytes,
            output_format,
        } => {
            if binary {
                let byte_range = match bytes.as_deref() {
                    None => None,
                    Some(s) => Some(cmd::read::parse_bytes_arg(s)?),
                };
                let all_bytes = fs::read(&file)?;
                let slice: &[u8] = match byte_range {
                    None => &all_bytes[..],
                    Some((lo, hi)) => {
                        let lo = (lo.saturating_sub(1) as usize).min(all_bytes.len());
                        let hi = (hi as usize).min(all_bytes.len());
                        &all_bytes[lo..hi]
                    }
                };
                if let Some(ref fmt) = output_format {
                    let encoded = data_format::encode(fmt, slice);
                    out.emit("readex", None, None, format_args!("{}", encoded));
                    return Ok(());
                }
                out.emit_binary_ln("readex", None, None, slice);
                Ok(())
            } else {
                let lines_range = match lines.as_deref() {
                    None => None,
                    Some(s) => Some(cmd::read::parse_lines_arg(s)?),
                };
                let output_encoding = if utf8 {
                    OutputEncoding::Utf8
                } else {
                    OutputEncoding::Preserve
                };
                let bom_policy = bom.unwrap_or_default();
                let mut buf: Vec<u8> = Vec::new();
                let mut notes_buf: Vec<u8> = Vec::new();
                let notes: Option<&mut dyn std::io::Write> = if mojibake_advisory_enabled {
                    Some(&mut notes_buf)
                } else {
                    None
                };
                cmd::readex::run(
                    &file,
                    lines_range,
                    numbers,
                    output_encoding,
                    bom_policy,
                    &mut buf,
                    IoMode::Mmap,
                    notes,
                )?;
                if !notes_buf.is_empty() {
                    let _ = shell.err().write_all(&notes_buf);
                }
                let content =
                    String::from_utf8(buf).map_err(|e| format!("readex: non-UTF-8 output: {e}"))?;
                out.emit("readex", None, None, format_args!("{}", content));
                Ok(())
            }
        }

        Commands::MoveBlock {
            source,
            dest,
            start_pattern,
            end_pattern,
            dest_header,
        } => {
            let result = match cmd::move_block::run(
                &source,
                &dest,
                &start_pattern,
                end_pattern.as_deref(),
                dest_header.as_deref(),
            ) {
                Ok(r) => r,
                Err(e) => {
                    let _ = shell.error(e);
                    let _ = shell.emit_finished(false);
                    std::process::exit(2);
                }
            };
            if shell.is_json() {
                out.emit_json(
                    "move-block",
                    None,
                    None,
                    &serde_json::json!({
                        "reason": "data",
                        "subcommand": "move-block",
                        "moved_lines": result.moved_lines,
                        "source_file": result.source_file,
                        "dest_file": result.dest_file,
                        "rendered": format!(
                            "Moved {} line{} from {} to {}\n",
                            result.moved_lines,
                            if result.moved_lines == 1 { "" } else { "s" },
                            result.source_file,
                            result.dest_file,
                        ),
                    }),
                );
            } else {
                shell.status(
                    "move-block",
                    format!(
                        "Moved {} line{}: {} \u{2192} {}",
                        result.moved_lines,
                        if result.moved_lines == 1 { "" } else { "s" },
                        result.source_file,
                        result.dest_file,
                    ),
                )?;
            }
            Ok(())
        }

        Commands::Append {
            file,
            data,
            validate,
            diff,
            line_ending,
            allow_mojibake,
        } => {
            let le_override = match line_ending.as_deref() {
                None => None,
                Some("lf") => Some(harrier::encoding::LineEnding::Lf),
                Some("crlf") => Some(harrier::encoding::LineEnding::CrLf),
                Some("cr") => Some(harrier::encoding::LineEnding::Cr),
                Some(other) => {
                    return Err(format!(
                        "--line-ending: unrecognised value {other:?}; expected lf, crlf, or cr"
                    )
                    .into())
                }
            };

            // Collect the text to append: from --data or from stdin.
            let new_text: String = match data {
                Some(s) => s,
                None => {
                    let mut s = String::new();
                    use std::io::Read;
                    io::stdin()
                        .read_to_string(&mut s)
                        .map_err(|e| format!("append: reading stdin: {e}"))?;
                    s
                }
            };

            // --diff: preview-only mode, no file modification.
            if diff {
                let mut diff_buf: Vec<u8> = Vec::new();
                cmd::append::run(
                    &file,
                    &new_text,
                    le_override,
                    Some(&mut diff_buf),
                    IoMode::Mmap,
                    if allow_mojibake {
                        mojibake::WritePolicy::permissive()
                    } else {
                        mojibake::WritePolicy::default()
                    },
                )?;
                if !diff_buf.is_empty() {
                    let content = String::from_utf8_lossy(&diff_buf).into_owned();
                    out.emit_json(
                        "append",
                        None,
                        None,
                        &serde_json::json!({
                            "reason": "diff",
                            "subcommand": "append",
                            "content": content,
                            "rendered": content,
                        }),
                    );
                }
                return Ok(());
            }

            // Run validate guards before any file modification.
            cmd::validate::run_all(&validate, &file, false, IoMode::Mmap)?;

            cmd::append::run(
                &file,
                &new_text,
                le_override,
                None,
                IoMode::Mmap,
                if allow_mojibake {
                    mojibake::WritePolicy::permissive()
                } else {
                    mojibake::WritePolicy::default()
                },
            )?;
            shell.status("append", format!("{}: content appended", file.display()))?;
            Ok(())
        }

        Commands::Count {
            file,
            lines,
            words,
            chars,
            bytes,
            pattern,
            label,
            stats,
        } => {
            // Stats are always emitted in JSON mode so JSON consumers can
            // rely on the encoding/bom/line_ending fields being present.
            let effective_stats = stats || json_mode;
            cmd::count::run(
                &file,
                lines,
                words,
                chars,
                bytes,
                &pattern,
                &label,
                effective_stats,
                out,
                IoMode::Mmap,
            )
        }

        Commands::Find {
            pattern,
            path,
            patterns,
            paths,
            all_match,
            fixed_strings,
            ignore_case,
            numbers,
            count,
            invert,
            multiline,
            after,
            before,
        } => {
            // Merge positional shorthand with repeatable flag values.
            let mut all_patterns: Vec<String> = Vec::new();
            if let Some(p) = pattern {
                all_patterns.push(p);
            }
            all_patterns.extend(patterns);

            let mut all_paths: Vec<String> = Vec::new();
            if let Some(p) = path {
                all_paths.push(p);
            }
            all_paths.extend(paths);

            // Validate at least one pattern and one path are provided.
            if all_patterns.is_empty() {
                return Err("find: at least one pattern is required \
                     (positional PATTERN or --pattern)"
                    .into());
            }
            if all_paths.is_empty() {
                return Err("find: at least one path is required \
                     (positional PATH or --path)"
                    .into());
            }

            let pattern_refs: Vec<&str> = all_patterns.iter().map(String::as_str).collect();
            let path_refs: Vec<&str> = all_paths.iter().map(String::as_str).collect();

            let mut buf: Vec<u8> = Vec::new();
            let mut walk_warnings: Vec<String> = Vec::new();
            let result = cmd::find::run_with_policy(
                &path_refs,
                &pattern_refs,
                fixed_strings,
                multiline,
                ignore_case,
                all_match,
                invert,
                before,
                after,
                count,
                numbers,
                &mut buf,
                IoMode::Mmap,
                on_error_mode,
                &mut walk_warnings,
            );
            for w in &walk_warnings {
                let _ = shell.warn(w);
            }
            match result {
                Ok(r) => {
                    let content = String::from_utf8(buf)
                        .map_err(|e| format!("find: non-UTF-8 output: {e}"))?;
                    out.emit("find", None, None, format_args!("{content}"));
                    if r.total_matches == 0 {
                        std::process::exit(1);
                    }
                    Ok(())
                }
                Err(e) => {
                    let _ = shell.error(e);
                    let _ = shell.emit_finished(false);
                    std::process::exit(2);
                }
            }
        }

        Commands::Doctor {
            paths,
            format,
            fix,
            quiet,
        } => {
            let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
            let opts = cmd::doctor::DoctorOptions {
                format: if format == "json" {
                    cmd::doctor::DoctorFormat::Json
                } else {
                    cmd::doctor::DoctorFormat::Human
                },
                fix: match fix.as_deref() {
                    Some("peel") => cmd::doctor::DoctorFix::Peel,
                    _ => cmd::doctor::DoctorFix::None,
                },
                quiet,
            };

            // Buffer the doctor output and route it through the standard
            // `Output` channel so JSON / human modes both work uniformly.
            let mut buf: Vec<u8> = Vec::new();
            let mut walk_warnings: Vec<String> = Vec::new();
            let report = cmd::doctor::run_with_policy(
                &path_refs,
                opts,
                &mut buf,
                IoMode::Mmap,
                on_error_mode,
                &mut walk_warnings,
            )?;
            for w in &walk_warnings {
                let _ = shell.warn(w);
            }
            let content = String::from_utf8(buf)
                .map_err(|e| format!("doctor: non-UTF-8 output: {e}"))?;
            out.emit("doctor", None, None, format_args!("{content}"));
            if report.total_issues() > 0 {
                std::process::exit(1);
            }
            Ok(())
        }

        Commands::Copy {
            source,
            dest,
            recursive,
            overwrite,
        } => {
            let opts = cmd::copy::CopyOptions {
                recursive,
                overwrite,
                on_error: on_error_mode,
            };
            let report = cmd::copy::run(&source, &dest, opts, shell)?;
            if json_mode {
                out.emit_json(
                    "copy",
                    None,
                    None,
                    &serde_json::json!({
                        "reason": "data",
                        "subcommand": "copy",
                        "copied": report.copied,
                        "skipped": report.skipped,
                        "warnings": report.warnings,
                        "rendered": format!(
                            "copied {} file{}, skipped {}, {} warning{}",
                            report.copied,
                            if report.copied == 1 { "" } else { "s" },
                            report.skipped,
                            report.warnings,
                            if report.warnings == 1 { "" } else { "s" },
                        ),
                    }),
                );
            } else {
                shell.status(
                    "copy",
                    format!(
                        "{} copied, {} skipped, {} warning{}",
                        report.copied,
                        report.skipped,
                        report.warnings,
                        if report.warnings == 1 { "" } else { "s" },
                    ),
                )?;
            }
            Ok(())
        }

        Commands::Render {
            output,
            template,
            template_file,
            var,
            missing,
            allow_mojibake,
        } => {
            use std::collections::BTreeMap;
            let mut vars: BTreeMap<String, String> = BTreeMap::new();
            for v in &var {
                let (k, val) = cmd::render::parse_var(v).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                vars.insert(k, val);
            }
            let policy = match missing.as_str() {
                "empty" => cmd::render::MissingPolicy::Empty,
                "leave" => cmd::render::MissingPolicy::Leave,
                _ => cmd::render::MissingPolicy::Error,
            };
            // If neither --template nor --template-file: read stdin.
            let stdin_template: Option<String> = if template.is_none() && template_file.is_none() {
                use std::io::Read;
                let mut s = String::new();
                io::stdin()
                    .read_to_string(&mut s)
                    .map_err(|e| format!("render: reading template from stdin: {e}"))?;
                Some(s)
            } else {
                None
            };
            let report = cmd::render::run(
                &output,
                template.as_deref(),
                template_file.as_deref(),
                stdin_template.as_deref(),
                &vars,
                policy,
                IoMode::Mmap,
                if allow_mojibake {
                    mojibake::WritePolicy::permissive()
                } else {
                    mojibake::WritePolicy::default()
                },
            )?;
            shell.status(
                "render",
                format!(
                    "{}: {} substitution{} ({} unique token{} referenced)",
                    output.display(),
                    report.substitutions,
                    if report.substitutions == 1 { "" } else { "s" },
                    report.referenced,
                    if report.referenced == 1 { "" } else { "s" },
                ),
            )?;
            Ok(())
        }

        Commands::Setup { inject } => match inject {
            None => {
                out.emit("setup", None, None, format_args!("{}", cmd::setup::full_block()));
                Ok(())
            }
            Some(path) => {
                let (updated, replaced) = cmd::setup::inject(&path, IoMode::Mmap)?;
                let verb = if !updated {
                    "already up to date"
                } else if replaced {
                    "block replaced"
                } else {
                    "block appended"
                };
                shell.status("setup", format!("{}: {verb}", path.display()))?;
                Ok(())
            }
        },
    }
}

/// Parse a `--data-length` VALUE: a decimal integer or a `0x`/`0X`-prefixed
/// hexadecimal integer.  Returns an error on invalid input.
fn parse_data_length(s: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let n = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        usize::from_str_radix(hex, 16)
            .map_err(|_| format!("--data-length: invalid hex value {s:?}"))?
    } else {
        s.parse::<usize>()
            .map_err(|_| format!("--data-length: invalid decimal value {s:?}"))?
    };
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::parse_data_length;

    // ── Decimal inputs ────────────────────────────────────────────────────────

    #[test]
    fn decimal_zero() {
        assert_eq!(parse_data_length("0").unwrap(), 0);
    }

    #[test]
    fn decimal_one() {
        assert_eq!(parse_data_length("1").unwrap(), 1);
    }

    #[test]
    fn decimal_small() {
        assert_eq!(parse_data_length("26").unwrap(), 26);
    }

    #[test]
    fn decimal_large() {
        assert_eq!(parse_data_length("1048576").unwrap(), 1_048_576);
    }

    #[test]
    fn decimal_max_usize() {
        // usize::MAX in decimal should parse correctly.
        assert_eq!(
            parse_data_length(&usize::MAX.to_string()).unwrap(),
            usize::MAX
        );
    }

    // ── Hex 0x prefix ────────────────────────────────────────────────────────

    #[test]
    fn hex_lowercase_prefix() {
        // 0x1A = 26
        assert_eq!(parse_data_length("0x1A").unwrap(), 26);
    }

    #[test]
    fn hex_uppercase_prefix() {
        // 0X1a = 26
        assert_eq!(parse_data_length("0X1a").unwrap(), 26);
    }

    #[test]
    fn hex_zero() {
        assert_eq!(parse_data_length("0x0").unwrap(), 0);
    }

    #[test]
    fn hex_all_lowercase_digits() {
        // 0xff = 255
        assert_eq!(parse_data_length("0xff").unwrap(), 255);
    }

    #[test]
    fn hex_all_uppercase_digits() {
        assert_eq!(parse_data_length("0xFF").unwrap(), 255);
    }

    #[test]
    fn hex_multi_byte() {
        // 0x200 = 512
        assert_eq!(parse_data_length("0x200").unwrap(), 512);
    }

    #[test]
    fn hex_word_size() {
        // 0x10000 = 65536
        assert_eq!(parse_data_length("0x10000").unwrap(), 65536);
    }

    // ── Error cases ───────────────────────────────────────────────────────────

    #[test]
    fn empty_string_is_error() {
        assert!(parse_data_length("").is_err());
    }

    #[test]
    fn negative_decimal_is_error() {
        assert!(parse_data_length("-1").is_err());
    }

    #[test]
    fn float_is_error() {
        assert!(parse_data_length("3.14").is_err());
    }

    #[test]
    fn hex_no_digits_is_error() {
        assert!(parse_data_length("0x").is_err());
    }

    #[test]
    fn hex_invalid_char_is_error() {
        assert!(parse_data_length("0xGG").is_err());
    }

    #[test]
    fn decimal_with_trailing_garbage_is_error() {
        assert!(parse_data_length("10abc").is_err());
    }

    #[test]
    fn plain_hex_without_prefix_is_error() {
        // "1A" without 0x prefix is not a valid decimal — must fail.
        assert!(parse_data_length("1A").is_err());
    }

    #[test]
    fn whitespace_is_error() {
        assert!(parse_data_length(" 10").is_err());
    }
}
