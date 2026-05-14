// Copyright (c) 2026, Michael Grier

//! Tool definitions and dispatch for the `tpu-mcp` MCP server.
//!
//! Each tool calls into the `tpu` library directly via the `call_*` dispatch
//! functions in this module.  File-reading tools return the raw file content
//! as plain text.  Mutating and introspection tools return a JSON-encoded
//! result object.  On failure all tools return an MCP error response.
//!
//! Tool set:
//! - `read_file`         — read a text file with encoding/line-ending normalisation
//! - `write_file`        — write text, preserving the file's existing encoding/line endings
//! - `replace_in_file`   — in-place regex substitution on a normalised LF view
//! - `edit_file`         — targeted in-place edits at known line numbers or byte offsets
//! - `read_file_binary`  — read raw bytes as a 7-bit-clean escaped string
//! - `read_file_escaped` — read text as a single flat escaped line (7-bit safe)
//! - `validate_file`     — assert that a specific location in a file matches a value
//! - `read_head`         — emit the first N lines or N bytes of a file (with optional line numbers)
//! - `read_tail`         — emit the last N lines or N bytes of a file (with optional line numbers)
//! - `count_file`        — count lines, words, characters, bytes, and pattern matches in a file
//! - `append_file`       — append text to an existing file, preserving its encoding and line endings
//!
//! ## Exception: `validate_file`
//!
//! `tpu` has no standalone `validate` subcommand; `--validate` is a pre-write
//! guard on `tpu write`.  `validate_file` therefore calls the tpu library
//! directly rather than via a subprocess.

use std::{path::Path, time::SystemTime};

use serde_json::Value;

// -- tool list -----------------------------------------------------------------

/// Names of every tool exposed by [`list()`], in advertising order.
///
/// Maintained as a static slice so that the startup banner ([`tool_names`])
/// does not pay the cost of constructing the full `tools/list` JSON payload
/// just to extract names. The unit test `tool_names_match_list_payload`
/// (runs in both debug and release test builds) keeps this in sync with
/// [`list()`].
pub const TOOL_NAMES: &[&str] = &[
    "tpu_read_file",
    "tpu_write_file",
    "tpu_replace_in_file",
    "tpu_edit_file",
    "tpu_read_file_binary",
    "tpu_read_file_escaped",
    "tpu_validate_file",
    "tpu_read_head",
    "tpu_read_tail",
    "tpu_count_file",
    "tpu_append_file",
    "tpu_find",
    "tpu_copy_file",
    "tpu_render_file",
    "tpu_setup",
    "tpu_stat_file",
];

/// Return the names (in advertising order) of every tool exposed by [`list()`].
///
/// Used by `main` to emit a startup `advertising tools: ...` log line so the
/// user can see at a glance which tools the running server actually exposes
/// without having to wait for the client's `tools/list` round-trip. Cheap:
/// returns a fixed static slice, no allocation or JSON construction.
pub fn tool_names() -> &'static [&'static str] {
    TOOL_NAMES
}

/// Return the MCP `tools/list` payload (an array of tool descriptors).
pub fn list() -> Value {
    serde_json::json!([
        {
            "name": "tpu_read_file",
            "description":
                "Read a text file and return its content as UTF-8 with LF line endings, \
                 regardless of the file's native encoding (UTF-8, UTF-16LE/BE, \
                 Windows-1252, Shift-JIS, …) or native line endings (LF, CRLF, CR). \
                 Prefer this over PowerShell Get-Content or shell cat to avoid \
                 encoding corruption. Optionally restrict to a line range and/or \
                 prefix each line with its 1-based number. \
                 Note: if the decoded text appears to contain mojibake (the canonical \
                 Latin-1, punctuation, box-drawing, NBSP, or double-encoded fingerprints), \
                 the read still succeeds; run `tpu doctor` (or call this tool from a \
                 CLI shell) to diagnose and optionally repair the file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Absolute path to the file to read."
                    },
                    "lines": {
                        "type": "string",
                        "description":
                            "Optional 1-based inclusive line range: 'N' for a single line \
                             or 'N-M' for a range. Omit to return the entire file."
                    },
                    "numbers": {
                        "type": "boolean",
                        "description":
                            "If true, prefix each output line with its 1-based line number \
                             (right-aligned, 6 digits, followed by two spaces)."
                    }
                },
                "required": ["file"]
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "tpu_write_file",
            "description":
                "Write UTF-8/LF text to a file, preserving the target file's existing \
                 encoding (UTF-8, UTF-16LE/BE, Windows-1252, …) and line endings \
                 (LF or CRLF). For new files, UTF-8/LF is used. The original file is \
                 atomically backed up to <file>.bak before writing. Prefer this over \
                 PowerShell Set-Content or Out-File to avoid encoding corruption.\n\n\
                 ESCAPING: 'content' is the LITERAL text to write. The JSON-RPC \
                 transport already handles JSON string escaping; do not add a second \
                 layer. To insert a newline put a real newline in the JSON string \
                 (encoded by JSON as \\n on the wire). To insert the two literal \
                 characters backslash + n, send a literal backslash followed by 'n' \
                 (encoded by JSON as \\\\n). When in doubt, treat 'content' as if you \
                 were typing directly into the file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Absolute path of the file to write."
                    },
                    "content": {
                        "type": "string",
                        "description":
                            "Full UTF-8 text content to write. Any CRLF or bare CR line \
                             endings are normalized to LF before processing. \
                             Line endings are then converted to match the target \
                             file's existing convention unless line_ending is specified."
                    },
                    "line_ending": {
                        "type": "string",
                        "enum": ["lf", "crlf", "cr"],
                        "description":
                            "Override the output line ending. Omit to preserve the file's \
                             existing convention. Cannot be used with binary content."
                    },
                    "diff": {
                        "type": "boolean",
                        "description":
                            "If true, emit a unified diff of the changes to stdout after \
                             the file is successfully written. When the content is identical \
                             to the existing file the diff output is empty. Default: false."
                    },
                    "validate": {
                        "type": "array",
                        "description":
                            "Pre-write guards. Each entry is an object with 'selector' and \
                             'value'. All validations run before the write; any failure \
                             leaves the file unchanged.\n\
                             Text selectors: 'line:N' (exact match), 'line-contains:N' \
                             (substring match).\n\
                             Binary selectors: 'bytes:OFFSET-END', 'md5:OFFSET-END', \
                             'crc32:OFFSET-END'.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "selector": { "type": "string" },
                                "value":    { "type": "string" }
                            },
                            "required": ["selector", "value"]
                        }
                    },
                    "allow_mojibake": {
                        "type": "boolean",
                        "description":
                            "If true, disable the write-time mojibake guard for this call. \
                             By default, a write whose content introduces new mojibake \
                             patterns (any of the canonical Latin-1, punctuation, \
                             box-drawing, NBSP, or double-encoded fingerprints) relative \
                             to the file's prior content is rejected with an error.  \
                             Pre-existing damage is ignored; only newly-introduced \
                             matches trigger a refusal.  Use this only when intentionally \
                             writing curated mojibake fixtures.  Default: false."
                    }
                },
                "required": ["file", "content"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "tpu_replace_in_file",
            "description":
                "Perform an in-place regex substitution on a file. The pattern is matched \
                 against a LF-normalised view so CRLF is transparent — \\n in the pattern \
                 always means line feed. Uses Rust regex::bytes syntax. Capture groups: \
                 $0 (whole match), $1/$2/…, $name. Use $$ for a literal dollar sign. \
                 The original file is backed up to <file>.bak before writing. \
                 Use count:true to count matches without modifying the file. \
                 Use dry_run:true to preview changes as a unified diff without writing.\n\n\
                 ESCAPING — RECOMMENDED DEFAULT: when the search target is literal text \
                 (code, JSON, structured data, anything containing . ( ) [ ] { } * + ? | ^ $ \\), \
                 set fixed_strings:true and send the unescaped text. This avoids regex \
                 escaping entirely and is almost always what you want.\n\n\
                 ESCAPING — 'pattern' (regex mode, fixed_strings:false): escape ONLY \
                 regex metacharacters. Do NOT add an extra layer for JSON; the transport \
                 already handles that.\n\n\
                 ESCAPING — 'replacement': capture refs use $1, $name, $$ (literal $). \
                 The sequences \\n, \\r, \\t, \\\\ are expanded to LF / CR / TAB / \\ \
                 before substitution; all other \\X pass through unchanged. Either a \
                 real newline in the JSON string OR the two characters backslash+n will \
                 produce a newline in the output — both are accepted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Absolute path of the file to modify."
                    },
                    "pattern": {
                        "type": "string",
                        "description":
                            "Regex pattern in regex::bytes syntax applied to the LF-normalised \
                             content. Use (?s) for dot-all (match across lines). \
                             If the search target contains regex metacharacters such as \
                             `{`, `}`, `(`, `)`, `[`, `.`, `*`, `+`, or `?` \
                             (common in code, JSON, or structured text), set \
                             fixed_strings:true instead of manually escaping them."
                    },
                    "replacement": {
                        "type": "string",
                        "description":
                            "Replacement template. $0 is the whole match; $1/$name are \
                             numbered/named capture groups; $$ is a literal dollar sign. \
                             Any CRLF or bare CR in the replacement text is normalized \
                             to LF before substitution. \
                             Standard C-style backslash escapes are expanded before the \
                             regex engine sees the replacement: \\n becomes a newline, \
                             \\t a tab, \\r a carriage return, \\\\ a single backslash. \
                             All other \\X sequences are passed through unchanged so that \
                             capture-group syntax ($1, $name, $$) is not affected."
                    },
                    "multiline": {
                        "type": "boolean",
                        "description":
                            "If true, prepend (?m) to the pattern so ^ and $ match at every \
                             LF boundary rather than only at start/end of file. Default: false."
                    },
                    "line_ending": {
                        "type": "string",
                        "enum": ["lf", "crlf", "cr"],
                        "description":
                            "Override the output line ending for the replacement output. \
                             Omit to preserve the file's existing convention."
                    },
                    "diff": {
                        "type": "boolean",
                        "description":
                            "If true, emit a unified diff of the changes to stdout after \
                             the substitution is successfully applied. When no matches are \
                             found the diff output is empty. Default: false."
                    },
                    "count": {
                        "type": "boolean",
                        "description":
                            "If true, count the number of substitutions that would be made \
                             and return the count. The file is not modified. \
                             Mutually exclusive with diff and dry_run. Default: false."
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description":
                            "If true, compute the substitution in memory and emit a unified \
                             diff without modifying the file. Returns non-zero when changes \
                             would be made. Mutually exclusive with count. Default: false."
                    },
                    "fixed_strings": {
                        "type": "boolean",
                        "description":
                            "Treat `pattern` as a fixed literal string; disable all regex \
                             metacharacters. Use whenever the search target contains `{`, \
                             `}`, `(`, `)`, `[`, `.`, `*`, `+`, or `?` � for example \
                             when replacing an exact code block, a function call, or any \
                             structured text. Equivalent to -F in grep."
                    },
                    "allow_mojibake": {
                        "type": "boolean",
                        "description":
                            "If true, disable the write-time mojibake guard for this call. \
                             By default, a substitution whose result introduces new mojibake \
                             patterns (any of the canonical Latin-1, punctuation, \
                             box-drawing, NBSP, or double-encoded fingerprints) relative \
                             to the file's prior content is rejected.  Pre-existing matches \
                             are ignored.  Default: false."
                    }
                },
                "required": ["file", "pattern", "replacement"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "tpu_edit_file",
            "description":
                "Make targeted in-place edits at known positions (1-based line numbers in \
                 text mode, 0-based byte offsets in binary mode). Prefer this over hand-\
                 rolled PowerShell or shell splice pipelines whenever you have a line \
                 number you trust — the edit is atomic, encoding-preserving, and backed \
                 by the same .bak / mojibake-guard machinery as tpu_write_file. \
                 When the target text is unique and you have just read it, prefer \
                 tpu_replace_in_file with fixed_strings:true instead, because line \
                 numbers can shift between reads.\n\n\
                 All operation positions reference the original file; multiple ops in one \
                 call are applied without interference. The original file is backed up \
                 to <file>.bak before writing.\n\n\
                 ESCAPING (text mode): each op's 'data' is LITERAL text. The JSON \
                 transport already handles escaping; do not add a second layer. Put a \
                 real newline in the JSON string for a newline in the file. CRLF/CR in \
                 'data' is normalised to LF before the edit, then re-encoded to match \
                 the file's line-ending convention. In binary mode, 'data' is raw bytes \
                 (or hex/base64 if data_format is set) with no escaping or normalisation.\n\n\
                 Each entry in 'ops' must have:\n\
                   op          — 'delete', 'insert', or 'splice'\n\
                   range       — 'N' or 'N-M' (required for delete/splice)\n\
                   offset      — 'N' (required for insert; position to insert before)\n\
                   data        — text or encoded bytes (required for insert/splice)\n\
                   data_format — 'hex', 'base64', or 'encoded' (binary mode only, optional)\n\n\
                 Text mode (binary: false, default):\n\
                   Positions are 1-based line numbers. Data is UTF-8 text with LF endings.\n\n\
                 Binary mode (binary: true):\n\
                   Positions are 0-based byte offsets. Data is raw or encoded bytes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Absolute path of the file to edit."
                    },
                    "ops": {
                        "type": "array",
                        "description":
                            "List of edit operations. Each operation is an object with \
                             fields: op ('delete'|'insert'|'splice'), range (for delete/splice), \
                             offset (for insert), data (for insert/splice), \
                             data_format (optional, binary mode only).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "op": {
                                    "type": "string",
                                    "enum": ["delete", "insert", "splice"]
                                },
                                "range": {
                                    "type": "string",
                                    "description": "'N' or 'N-M' (for delete/splice)."
                                },
                                "offset": {
                                    "type": "string",
                                    "description": "'N' (for insert)."
                                },
                                "data": {
                                    "type": "string",
                                    "description": "Content to insert or splice in. \
                                     In text mode, any CRLF or bare CR is normalized to LF \
                                     before the edit is applied."
                                },
                                "data_format": {
                                    "type": "string",
                                    "enum": ["hex", "base64", "encoded"],
                                    "description": "Encoding of data (binary mode only)."
                                }
                            },
                            "required": ["op"]
                        }
                    },
                    "validate": {
                        "type": "array",
                        "description":
                            "Pre-edit guards. Each entry is an object with 'selector' and \
                             'value'. All validations run before any edit is applied; any \
                             failure leaves the file unchanged.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "selector": { "type": "string" },
                                "value":    { "type": "string" }
                            },
                            "required": ["selector", "value"]
                        }
                    },
                    "binary": {
                        "type": "boolean",
                        "description":
                            "If true, positions are 0-based byte offsets and no \
                             encoding/line-ending processing is applied. Default: false."
                    },
                    "diff": {
                        "type": "boolean",
                        "description":
                            "If true, include a unified diff of the changes in the result \
                             (text mode only). Default: false."
                    },
                    "line_ending": {
                        "type": "string",
                        "enum": ["lf", "crlf", "cr"],
                        "description":
                            "Override the output line ending in text mode. Omit to preserve \
                             the file's existing convention. Conflicts with binary: true."
                    },
                    "allow_mojibake": {
                        "type": "boolean",
                        "description":
                            "If true, disable the write-time mojibake guard for this call. \
                             By default, an edit whose result introduces new mojibake \
                             patterns relative to the file's prior content is rejected.  \
                             Pre-existing matches are ignored.  Default: false."
                    }
                },
                "required": ["file", "ops"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true }
        },
        {
            "name": "tpu_read_file_binary",
            "description":
                "Read raw bytes from a file and return them as a 7-bit-clean escaped \
                 string. Non-printable bytes are encoded as \\xHH (lowercase hex); \
                 printable ASCII passes through unchanged. Use this for binary files \
                 (executables, images, archives) or to inspect exact byte values. \
                 For text files, prefer tpu_read_file. \
                 When the hash parameter is supplied, the output is a JSON object \
                 containing the base64-encoded content and a hashes array.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Absolute path to the file to read."
                    },
                    "bytes": {
                        "type": "string",
                        "description":
                            "Optional 1-based inclusive byte range: 'N' for a single byte \
                             or 'N-M' for a range. Omit to read the entire file."
                    },
                    "hash": {
                        "type": "array",
                        "description":
                            "Integrity hash specs to compute over byte ranges. Each entry \
                             is an ALGO:RANGE string. ALGO is 'crc32' or 'md5'. \
                             RANGE is 'START-END' (0-based decimal or 0x-prefixed hex byte \
                             offsets); use '$' or 'EOF' for end-of-file. \
                             Example: ['crc32:0-$', 'md5:0-100']. \
                             When this array is non-empty the response is a single NDJSON \
                             line containing 'encoding', 'content' (base64), and 'hashes'.",
                        "items": { "type": "string" }
                    }
                },
                "required": ["file"]
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "tpu_read_file_escaped",
            "description":
                "Read a text file and return its decoded content as a single 7-bit-clean \
                 ASCII line. Every non-printable character — including all line breaks — is \
                 encoded as a backslash escape sequence (\\n, \\r, \\t, \\uXXXX, \\UXXXXXXXX). \
                 Use this when file content must survive transport through a context that \
                 cannot safely carry raw control characters or multi-line strings \
                 (shell variables, JSON fields, agent tool output).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Absolute path to the file to read."
                    },
                    "lines": {
                        "type": "string",
                        "description":
                            "Optional 1-based inclusive line range: 'N' or 'N-M'. \
                             Omit to include the entire file."
                    },
                    "numbers": {
                        "type": "boolean",
                        "description":
                            "If true, prefix each source line's escaped content with its \
                             1-based line number."
                    }
                },
                "required": ["file"]
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "tpu_validate_file",
            "description":
                "Assert that a specific location in a file matches an expected value. \
                 USE THIS BEFORE ANY DESTRUCTIVE WRITE that depends on the file being in \
                 a known state — or pass the equivalent check inline via the `validate` \
                 array supported by tpu_write_file / tpu_replace_in_file / tpu_edit_file \
                 / tpu_append_file (those run the same checks atomically before the \
                 write). Returns 'validation passed' on success, or an error describing \
                 the mismatch.\n\n\
                 Text selectors:\n\
                   line:N            — line N (1-based) must exactly equal the value\n\
                   line-contains:N   — line N must contain the value as a substring\n\n\
                 Binary selectors (set is_binary: true, or omit for auto-detection):\n\
                   bytes:OFFSET-END  — raw bytes [OFFSET, END) must equal value as contiguous hex\n\
                   md5:OFFSET-END    — MD5 of [OFFSET, END) must equal value (32 hex chars)\n\
                   crc32:OFFSET-END  — CRC32 of [OFFSET, END) must equal value (8 hex chars)\n\n\
                 OFFSET and END are 0-based byte offsets (decimal or 0x-prefixed hex); \
                 use $ or EOF for end-of-file.\n\n\
                 ESCAPING — 'value': for line: and line-contains: selectors, this is \
                 LITERAL text (the JSON transport handles string escaping; do not add \
                 a second layer). For bytes:, md5:, and crc32: selectors, 'value' is a \
                 lowercase hex string with no separators or 0x prefix.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Absolute path of the file to validate."
                    },
                    "selector": {
                        "type": "string",
                        "description":
                            "What to check: 'line:N', 'line-contains:N', \
                             'bytes:OFFSET-END', 'md5:OFFSET-END', or 'crc32:OFFSET-END'."
                    },
                    "value": {
                        "type": "string",
                        "description": "The expected value at the selected location."
                    },
                    "is_binary": {
                        "type": "boolean",
                        "description":
                            "Override binary-mode detection. When omitted, the selector \
                             prefix ('bytes:', 'md5:', 'crc32:') determines binary mode \
                             automatically."
                    }
                },
                "required": ["file", "selector", "value"]
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "tpu_read_head",
            "description":
                "Emit the first N lines or N bytes of a file. Prefer this over \
                 PowerShell `Get-Content -First` / `Select-Object -First` or shell \
                 `head` to avoid encoding corruption on UTF-16, Windows-1252, and \
                 Shift-JIS files.\n\n\
                 In line mode (default) the file's native encoding and line-ending \n\
                 convention are preserved in the output. If the file has fewer lines \n\
                 than requested, all lines are returned without error. \n\n\
                 In byte mode (bytes field set) the first N raw bytes are returned \n\
                 verbatim with no encoding or line-ending transformation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Absolute path to the file to read."
                    },
                    "lines": {
                        "type": "integer",
                        "description":
                            "Number of lines to emit (first N lines, 1-based). \n\
                             Default: 10. Mutually exclusive with 'bytes'."
                    },
                    "bytes": {
                        "type": "integer",
                        "description":
                            "Number of raw bytes to emit. Mutually exclusive with 'lines'. \n\
                             When set, no encoding or line-ending processing is applied."
                    },
                    "binary": {
                        "type": "boolean",
                        "description":
                            "Suppress encoding detection and treat the file as raw bytes. \n\
                             Only valid when 'bytes' is also set."
                    },
                    "numbers": {
                        "type": "boolean",
                        "description":
                            "Prefix each output line with its 1-based line number followed \n\
                             by a tab. Output always uses LF line endings when enabled. \n\
                             Mutually exclusive with 'bytes' and 'binary'."
                    }
                },
                "required": ["file"]
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "tpu_read_tail",
            "description":
                "Emit the last N lines or N bytes of a file. Prefer this over \
                 PowerShell `Get-Content -Tail` / `Select-Object -Last` or shell \
                 `tail` to avoid encoding corruption on UTF-16, Windows-1252, and \
                 Shift-JIS files.\n\n\
                 In line mode (default) the file's native encoding and line-ending \n\
                 convention are preserved in the output. If the file has fewer lines \n\
                 than requested, all lines are returned without error. \n\n\
                 In byte mode (bytes field set) the last N raw bytes are returned \n\
                 verbatim with no encoding or line-ending transformation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Absolute path to the file to read."
                    },
                    "lines": {
                        "type": "integer",
                        "description":
                            "Number of lines to emit (last N lines). \n\
                             Default: 10. Mutually exclusive with 'bytes'."
                    },
                    "bytes": {
                        "type": "integer",
                        "description":
                            "Number of raw bytes to emit from the end of the file. \
                             Mutually exclusive with 'lines'. \n\
                             When set, no encoding or line-ending processing is applied."
                    },
                    "binary": {
                        "type": "boolean",
                        "description":
                            "Suppress encoding detection and treat the file as raw bytes. \n\
                             Only valid when 'bytes' is also set."
                    },
                    "numbers": {
                        "type": "boolean",
                        "description":
                            "Prefix each output line with its absolute 1-based line number \n\
                             followed by a tab. Output always uses LF line endings when \n\
                             enabled. Mutually exclusive with 'bytes' and 'binary'."
                    }
                },
                "required": ["file"]
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "tpu_count_file",
            "description":
                "Count lines, words, characters, bytes, and/or regex pattern matches in a \
                 file. Prefer this over PowerShell `Measure-Object` / `(Get-Content).Count` \
                 or shell `wc` — counts are computed against the encoding-decoded view, \
                 so multi-byte UTF-8 characters and UTF-16 files give correct results. \
                 The file is decoded with encoding/line-ending normalisation before \
                 counting (same rules as tpu_read_file). \n\n\
                 When none of lines/words/chars/bytes is set, all four standard metrics are \
                 reported. Pattern matches are always reported when patterns are supplied. \n\n\
                 Set stats=true to include file metadata (encoding name, BOM presence, \
                 line-ending style) before the metric counts. Stats are always included in \
                 JSON output. \n\n\
                 Returns a JSON object with the requested counts and, for each named \
                 pattern, a 'patterns' array entry with the match count.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Absolute path to the file to count."
                    },
                    "lines": {
                        "type": "boolean",
                        "description":
                            "Include the line count in the output. \n\
                             When no metric flags are set all four standard metrics are \
                             reported; setting any flag reports only the selected metrics."
                    },
                    "words": {
                        "type": "boolean",
                        "description": "Include the word count in the output."
                    },
                    "chars": {
                        "type": "boolean",
                        "description": "Include the Unicode character count in the output."
                    },
                    "bytes": {
                        "type": "boolean",
                        "description": "Include the raw byte count in the output."
                    },
                    "patterns": {
                        "type": "array",
                        "description":
                            "Zero or more regex patterns to count in the file. Each entry \
                             must have a 'pattern' field (regex string) and an optional \
                             'label' field (display name). Match counts are always included \
                             regardless of the lines/words/chars/bytes flags.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "pattern": {
                                    "type": "string",
                                    "description": "Regex pattern to match."
                                },
                                "label": {
                                    "type": "string",
                                    "description": "Optional display label for this pattern."
                                }
                            },
                            "required": ["pattern"]
                        }
                    },
                    "stats": {
                        "type": "boolean",
                        "description":
                            "Emit file metadata (encoding name, BOM presence, line-ending \
                             style) before the metric counts. When used without other metric \
                             flags the default set (lines, words, chars, bytes) is still \
                             reported. Stats are always present in JSON output regardless \
                             of this flag."
                    },
                    "message_format": {
                        "type": "string",
                        "enum": ["human", "json"],
                        "description":
                            "Output format override. 'human' returns a rendered summary \
                             string; 'json' returns raw JSON. Omit to use the tpu default."
                    }
                },
                "required": ["file"]
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "tpu_append_file",
            "description":
                "Append UTF-8/LF text to an existing file, preserving its native encoding \
                 (UTF-8, UTF-16LE/BE, Windows-1252, …) and dominant line-ending convention \
                 (LF or CRLF).  The appended content is re-encoded and line endings are \
                 denormalised to match the file's convention.  The original file is atomically \
                 backed up to <file>.bak before writing. \n\n\
                 Use tpu_write_file to create new files; tpu_append_file requires the file to \
                 already exist.\n\n\
                 ESCAPING: 'content' is the LITERAL text to append. The JSON-RPC transport \
                 already handles JSON string escaping; do not add a second layer. To append \
                 a newline, put a real newline in the JSON string. Note that the file's \
                 last line may or may not already end in a newline — if you need a clean \
                 separation between the existing content and your appended text, prepend \
                 a newline to 'content' yourself.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Absolute path of the file to append to (must already exist)."
                    },
                    "content": {
                        "type": "string",
                        "description":
                            "UTF-8 text to append. Any CRLF or bare CR line endings \
                             are normalized to LF before processing. Line endings are \
                             then converted to match the target file's convention \
                             unless line_ending is specified."
                    },
                    "line_ending": {
                        "type": "string",
                        "enum": ["lf", "crlf", "cr"],
                        "description":
                            "Override the output line ending for the combined file. Omit to \
                             preserve the file's existing convention."
                    },
                    "diff": {
                        "type": "boolean",
                        "description":
                            "If true, emit a unified diff of what would be appended without \
                             modifying the file (dry-run / preview mode)."
                    },
                    "validate": {
                        "type": "array",
                        "description":
                            "Zero or more pre-append validation guards.  Each entry must have \
                             'selector' (e.g. 'line:1') and 'value' fields.  All validations \
                             run before any write; any failure leaves the file unchanged.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "selector": {
                                    "type": "string",
                                    "description": "Validation selector (e.g. 'line:1', 'line-contains:2')."
                                },
                                "value": {
                                    "type": "string",
                                    "description": "Expected value for the selector."
                                }
                            },
                            "required": ["selector", "value"]
                        }
                    },
                    "allow_mojibake": {
                        "type": "boolean",
                        "description":
                            "If true, disable the write-time mojibake guard for this call. \
                             By default, an append whose payload introduces new mojibake \
                             patterns relative to the file's prior content is rejected.  \
                             Pre-existing matches are ignored.  Default: false."
                    }
                },
                "required": ["file", "content"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "tpu_find",
            "description":
                "Search one or more files (or glob patterns) for lines matching one \
                 or more regex (or fixed-string) patterns.  Files are decoded with \
                 encoding and line-ending normalisation (same rules as tpu_read_file). \
                 Matched lines are emitted as UTF-8 to stdout with optional context \
                 lines (-A/-B), line-number prefixes, or a match count. \
                 Prefer this over PowerShell Select-String, grep, or rg to avoid \
                 encoding corruption on UTF-16LE/BE, Windows-1252, and Shift-JIS files. \n\n\
                 Exit codes: 0 = at least one match found, 1 = no matches, \
                 2 = error (bad pattern, unreadable file, etc.).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description":
                            "Primary regex (or fixed-string) pattern to search for. \
                             At least one of 'pattern' or 'patterns' must be supplied. \
                             If the search target contains regex metacharacters such as \
                             `{`, `}`, `(`, `)`, `[`, `.`, `*`, `+`, or `?` \
                             (common in code, JSON, or structured text), set \
                             fixed_strings:true instead of manually escaping them."
                    },
                    "patterns": {
                        "type": "array",
                        "description":
                            "Additional patterns.  Combined with 'pattern' (if present). \
                             By default lines matching ANY pattern are emitted (OR mode). \
                             Set all_match=true for AND mode.",
                        "items": { "type": "string" }
                    },
                    "path": {
                        "type": "string",
                        "description":
                            "Absolute file path or wax glob to search. \
                             At least one of 'path' or 'paths' must be supplied."
                    },
                    "paths": {
                        "type": "array",
                        "description":
                            "Additional file paths or wax globs. Combined with 'path' \
                             (if present).",
                        "items": { "type": "string" }
                    },
                    "all_match": {
                        "type": "boolean",
                        "description":
                            "When true, a line must match ALL supplied patterns to be \
                             emitted (AND mode).  Default false (OR mode)."
                    },
                    "fixed_strings": {
                        "type": "boolean",
                        "description":
                            "Treat every pattern as a fixed literal string rather than \
                             a regex.  Equivalent to -F in grep."
                    },
                    "ignore_case": {
                        "type": "boolean",
                        "description": "Case-insensitive matching."
                    },
                    "numbers": {
                        "type": "boolean",
                        "description":
                            "Prefix each output line with its 1-based line number \
                             followed by a colon. Format: N:content."
                    },
                    "count": {
                        "type": "boolean",
                        "description":
                            "Emit only a count of matching lines per file instead of the \
                             lines themselves. With multiple files a total line is also \
                             emitted."
                    },
                    "invert": {
                        "type": "boolean",
                        "description":
                            "Invert the match: emit lines that do NOT match (or, in \
                             all_match mode, lines that fail at least one pattern)."
                    },
                    "multiline": {
                        "type": "boolean",
                        "description":
                            "Enable multiline mode so that ^ and $ match at LF \
                             boundaries within the file content."
                    },
                    "after": {
                        "type": "integer",
                        "description":
                            "Number of context lines to emit after each matching line. \
                             Groups of context are separated by '--'."
                    },
                    "before": {
                        "type": "integer",
                        "description":
                            "Number of context lines to emit before each matching line."
                    },
                    "on_error": {
                        "type": "string",
                        "enum": ["warn", "fail"],
                        "description":
                            "How to handle walk errors when expanding globs (e.g. \
                             permission-denied directories). 'warn' (default) skips \
                             unreadable entries and continues; 'fail' aborts on the \
                             first walk error."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "tpu_copy_file",
            "description":
                "Copy a file or recursively copy a directory tree, preserving bytes \
                 verbatim (no encoding or line-ending transformation). \
                 Prefer this over PowerShell Copy-Item / shell `cp` when working in a \
                 workspace that uses tpu-mcp — copy is resilient by default: per-entry \
                 errors (unreadable directories, permission denied, write failures) \
                 produce a streamed warning record and the operation continues with \
                 the next entry. Set on_error:'fail' to restore the legacy 'abort on \
                 first error' behaviour.\n\n\
                 Modes:\n\
                   single file       — `source` is a file path, `dest` is a file path \
                                       or an existing directory.\n\
                   directory tree    — `source` is a directory path; pass recursive:true. \
                                       `dest` is created if needed and the tree is \
                                       mirrored beneath it.\n\
                   glob expansion    — `source` contains `*`, `?`, `[`, `{`. The matches \
                                       (relative to the current working directory) are \
                                       copied flat into `dest` (which must be a directory).\n\n\
                 By default, an existing destination file is skipped (and counted in \
                 the report). Pass overwrite:true to replace existing targets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "Source file path, directory path, or glob pattern."
                    },
                    "dest": {
                        "type": "string",
                        "description": "Destination file or directory."
                    },
                    "recursive": {
                        "type": "boolean",
                        "description":
                            "Recurse into directories. Required when `source` is a directory. \
                             Default: false."
                    },
                    "overwrite": {
                        "type": "boolean",
                        "description":
                            "Overwrite existing destination files. Without this, existing \
                             targets are skipped (and counted in the report). Default: false."
                    },
                    "on_error": {
                        "type": "string",
                        "enum": ["warn", "fail"],
                        "description":
                            "Per-entry error policy. 'warn' (default) continues past \
                             unreadable directories and write failures, returning a \
                             warnings count. 'fail' aborts on the first error."
                    }
                },
                "required": ["source", "dest"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "tpu_render_file",
            "description":
                "Populate an output file from a Mustache-style `{{TOKEN}}` template. \
                 Use this from Copilot Chat to scaffold files (READMEs, manifests, \
                 boilerplate code) without resorting to PowerShell here-strings or \
                 shell heredocs, which both round-trip non-ASCII content through the \
                 active code page and silently corrupt it.\n\n\
                 Template source: provide exactly one of `template` (inline string) or \
                 `template_file` (path to a template file decoded with the same rules \
                 as tpu_read_file).\n\n\
                 Tokens are written `{{NAME}}` (whitespace inside the braces is \
                 tolerated, e.g. `{{ NAME }}`). To emit literal braces, escape with \
                 a leading backslash: `\\{{`. Each token name must consist of ASCII \
                 letters, digits, '_' or '-'.\n\n\
                 The rendered text is written through the same atomic write path as \
                 tpu_write_file, so the destination receives the standard mojibake \
                 guard, .bak handling, and encoding preservation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "output": {
                        "type": "string",
                        "description": "Absolute path of the file to (re)create."
                    },
                    "template": {
                        "type": "string",
                        "description":
                            "Inline template string. Mutually exclusive with template_file."
                    },
                    "template_file": {
                        "type": "string",
                        "description":
                            "Absolute path to a template file. Mutually exclusive with template."
                    },
                    "vars": {
                        "type": "object",
                        "description":
                            "Token name → value map. Keys must match [A-Za-z0-9_-]+. \
                             Values are inserted literally (no further escaping).",
                        "additionalProperties": { "type": "string" }
                    },
                    "missing": {
                        "type": "string",
                        "enum": ["error", "empty", "leave"],
                        "description":
                            "What to do when the template references a token absent from \
                             `vars`: 'error' (default) lists every missing token and \
                             refuses the write; 'empty' substitutes the empty string; \
                             'leave' keeps the literal `{{NAME}}` placeholder in place."
                    },
                    "allow_mojibake": {
                        "type": "boolean",
                        "description":
                            "If true, disable the write-time mojibake guard for this call. \
                             Default: false."
                    }
                },
                "required": ["output"],
                "description":
                    "Exactly one of `template` or `template_file` must also be provided \
                     alongside `output`; calls that omit both will fail at runtime with \
                     a descriptive error."
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "tpu_setup",
            "description":
                "Emit (or inject) a canonical Markdown block that teaches Copilot to \
                 prefer this MCP server's `tpu_*` tools over PowerShell / shell file \
                 commands. Use this once per repository to keep `.github/copilot-instructions.md` \
                 (or any equivalent) up to date — re-running the tool replaces an \
                 existing managed block in place, so it is safe to invoke after every \
                 tpu-mcp upgrade.\n\n\
                 Without `target` the block is returned as the tool result. With \
                 `target` the named file is updated in place: an existing managed \
                 block (delimited by `<!-- tpu-mcp:setup:begin -->` / \
                 `<!-- tpu-mcp:setup:end -->`) is replaced; otherwise the block is \
                 appended after a single blank line. The destination file's encoding \
                 and line-ending convention are preserved.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description":
                            "Absolute path of the file to inject the guidance block into. \
                             Typical value: '<repo>/.github/copilot-instructions.md'. \
                             Omit to receive the block as the tool result without \
                             writing any file."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "tpu_stat_file",
            "description":
                "Return file metadata (size, modification time, creation time, read-only \
                 flag) as a JSON object. Call this immediately after a write when you need \
                 to confirm the change actually persisted: compare the returned \
                 mtime_epoch_ms against the value included in the response from \
                 tpu_write_file, tpu_replace_in_file, tpu_edit_file, or tpu_append_file. \
                 A stale or mismatched mtime after a write likely indicates Windows \
                 Defender interference and means the operation should be retried.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Absolute path of the file to stat."
                    }
                },
                "required": ["file"]
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        }
    ])
}

// -- dispatch ------------------------------------------------------------------

/// Call a named tool with the given JSON arguments.
///
/// On success returns the UTF-8 text for the MCP `content` array entry.
/// On failure returns an error that becomes an `isError: true` tool result.
pub fn call(
    name: &str,
    args: &Value,
    config: &ServerConfig,
) -> Result<String, Box<dyn std::error::Error>> {
    match name {
        "tpu_read_file" => call_read_file(args),
        "tpu_write_file" => call_write_file(args, config),
        "tpu_replace_in_file" => call_replace_in_file(args, config),
        "tpu_edit_file" => call_edit_file(args, config),
        "tpu_read_file_binary" => call_read_file_binary(args),
        "tpu_read_file_escaped" => call_read_file_escaped(args),
        "tpu_validate_file" => call_validate_file(args),
        "tpu_read_head" => call_read_head(args),
        "tpu_read_tail" => call_read_tail(args),
        "tpu_count_file" => call_count_file(args),
        "tpu_append_file" => call_append_file(args, config),
        "tpu_copy_file" => call_copy_file(args, config),
        "tpu_render_file" => call_render_file(args, config),
        "tpu_setup" => call_setup(args, config),
        "tpu_find" => call_find(args, config),
        "tpu_stat_file" => call_stat_file(args),
        _ => Err(format!("unknown tool: {name}").into()),
    }
}

// -- individual tool implementations ------------------------------------------

fn call_read_file(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let file = resolve_file_arg(args)?;
    let path = std::path::Path::new(&file);

    let lines_range = match args.get("lines").and_then(|v| v.as_str()) {
        None => None,
        Some(s) => Some(tpu::cmd::read::parse_lines_arg(s)?),
    };
    let numbers = args
        .get("numbers")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut buf: Vec<u8> = Vec::new();
    tpu::cmd::read::run(
        path,
        lines_range,
        numbers,
        tpu::encoding::OutputEncoding::Preserve,
        tpu::encoding::BomPolicy::default(),
        &mut buf,
        tpu::IoMode::Buffered,
        None,
    )?;
    Ok(String::from_utf8(buf).map_err(|e| format!("read: non-UTF-8 output: {e}"))?)
}

fn call_write_file(
    args: &Value,
    config: &ServerConfig,
) -> Result<String, Box<dyn std::error::Error>> {
    let file = resolve_file_arg(args)?;
    let content_raw = require_str(args, "content")?;
    let content = normalize_to_lf(content_raw);
    let path = std::path::Path::new(&file);

    let le_override = match args.get("line_ending").and_then(|v| v.as_str()) {
        None => None,
        Some(s) => Some(tpu::encoding::parse_line_ending(s)?),
    };

    // Run validate guards before any write.
    if let Some(validates) = args.get("validate").and_then(|v| v.as_array()) {
        let pairs = flatten_validate_pairs(validates)?;
        tpu::cmd::validate::run_all(&pairs, path, false, tpu::IoMode::Buffered)?;
    }

    let diff = args.get("diff").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut diff_buf: Vec<u8> = Vec::new();
    let diff_out: Option<&mut dyn std::io::Write> = if diff { Some(&mut diff_buf) } else { None };

    let policy = mojibake_policy_from_args(args);
    tpu::cmd::write::run(
        path,
        &content,
        tpu::encoding::OutputEncoding::Preserve,
        tpu::encoding::BomPolicy::default(),
        le_override,
        diff_out,
        tpu::IoMode::Buffered,
        policy,
    )?;
    delete_bak_if_exists(&file);
    let stamp = stamp_and_verify(path, config.verify_delay_ms)?;
    if diff && !diff_buf.is_empty() {
        Ok(String::from_utf8_lossy(&diff_buf).into_owned())
    } else {
        Ok(format!(
            "wrote '{}' [mtime={}, size={}]",
            file, stamp.mtime_epoch_ms, stamp.size
        ))
    }
}

fn call_replace_in_file(
    args: &Value,
    config: &ServerConfig,
) -> Result<String, Box<dyn std::error::Error>> {
    let file = resolve_file_arg(args)?;
    let pattern = require_str(args, "pattern")?;
    let replacement_raw = require_str(args, "replacement")?;
    let replacement_unescaped = unescape_replacement(replacement_raw);
    let replacement = normalize_to_lf(&replacement_unescaped);
    let path = std::path::Path::new(&file);

    let multiline = args
        .get("multiline")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let fixed_strings = args
        .get("fixed_strings")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let le_override = match args.get("line_ending").and_then(|v| v.as_str()) {
        None => None,
        Some(s) => Some(tpu::encoding::parse_line_ending(s)?),
    };
    let diff = args.get("diff").and_then(|v| v.as_bool()).unwrap_or(false);
    let count = args.get("count").and_then(|v| v.as_bool()).unwrap_or(false);
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut diff_buf: Vec<u8> = Vec::new();
    let diff_out: Option<&mut dyn std::io::Write> = if diff || dry_run {
        Some(&mut diff_buf)
    } else {
        None
    };

    let n = tpu::cmd::replace::run(
        path,
        pattern,
        replacement.as_bytes(),
        multiline,
        fixed_strings,
        le_override,
        diff_out,
        count,
        dry_run,
        tpu::IoMode::Buffered,
        mojibake_policy_from_args(args),
    )?;

    if count {
        return Ok(format!("count: {n}"));
    }
    if dry_run {
        if !diff_buf.is_empty() {
            return Ok(String::from_utf8_lossy(&diff_buf).into_owned());
        }
        return Ok(format!("no changes in '{file}'"));
    }
    // File was modified.
    delete_bak_if_exists(&file);
    let stamp = stamp_and_verify(Path::new(&file), config.verify_delay_ms)?;
    if diff && !diff_buf.is_empty() {
        Ok(String::from_utf8_lossy(&diff_buf).into_owned())
    } else {
        Ok(format!(
            "replaced in '{}' [mtime={}, size={}]",
            file, stamp.mtime_epoch_ms, stamp.size
        ))
    }
}

fn call_edit_file(
    args: &Value,
    config: &ServerConfig,
) -> Result<String, Box<dyn std::error::Error>> {
    let file = resolve_file_arg(args)?;
    let path = std::path::Path::new(&file);
    let binary = args
        .get("binary")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let le_override = match args.get("line_ending").and_then(|v| v.as_str()) {
        None => None,
        Some(s) => Some(tpu::encoding::parse_line_ending(s)?),
    };

    // Collect --data-format from the first op that specifies one.
    let data_format: Option<tpu::data_format::DataFormat> =
        if let Some(ops_arr) = args.get("ops").and_then(|v| v.as_array()) {
            ops_arr
                .iter()
                .find_map(|op| op.get("data_format").and_then(|v| v.as_str()))
                .map(parse_data_format)
                .transpose()?
        } else {
            None
        };

    // Run validate guards before any edit.
    if let Some(validates) = args.get("validate").and_then(|v| v.as_array()) {
        let pairs = flatten_validate_pairs(validates)?;
        tpu::cmd::validate::run_all(&pairs, path, binary, tpu::IoMode::Buffered)?;
    }

    // Parse ops into EditOp values.
    let mut ops: Vec<tpu::cmd::edit::EditOp> = Vec::new();
    if let Some(ops_arr) = args.get("ops").and_then(|v| v.as_array()) {
        for op in ops_arr {
            let op_name = op
                .get("op")
                .and_then(|v| v.as_str())
                .ok_or("op entry missing 'op' field")?;
            match op_name {
                "delete" => {
                    let range_s = op
                        .get("range")
                        .and_then(|v| v.as_str())
                        .ok_or("delete op missing 'range'")?;
                    let (start, end) = if binary {
                        tpu::cmd::edit::parse_byte_range(range_s)
                            .map_err(|e| format!("delete: {e}"))?
                    } else {
                        tpu::cmd::edit::parse_line_range(range_s)
                            .map_err(|e| format!("delete: {e}"))?
                    };
                    ops.push(tpu::cmd::edit::EditOp::Delete { start, end });
                }
                "insert" => {
                    let offset_s = op
                        .get("offset")
                        .and_then(|v| v.as_str())
                        .ok_or("insert op missing 'offset'")?;
                    let data_s = op
                        .get("data")
                        .and_then(|v| v.as_str())
                        .ok_or("insert op missing 'data'")?;
                    let offset = if binary {
                        tpu::cmd::edit::parse_byte_pos(offset_s)
                            .map_err(|e| format!("insert offset: {e}"))?
                    } else {
                        tpu::cmd::edit::parse_line_num(offset_s)
                            .map_err(|e| format!("insert offset: {e}"))?
                    };
                    let data = if let Some(ref fmt) = data_format {
                        tpu::data_format::decode(fmt, data_s)
                            .map_err(|e| format!("insert data decode: {e}"))?
                    } else {
                        data_s.as_bytes().to_vec()
                    };
                    // Normalize line endings to LF in text mode; binary data
                    // must not be altered.
                    let data = if binary {
                        data
                    } else {
                        normalize_bytes_to_lf(data)
                    };
                    ops.push(tpu::cmd::edit::EditOp::Insert { offset, data });
                }
                "splice" => {
                    let range_s = op
                        .get("range")
                        .and_then(|v| v.as_str())
                        .ok_or("splice op missing 'range'")?;
                    let data_s = op
                        .get("data")
                        .and_then(|v| v.as_str())
                        .ok_or("splice op missing 'data'")?;
                    let (start, end) = if binary {
                        tpu::cmd::edit::parse_byte_range(range_s)
                            .map_err(|e| format!("splice: {e}"))?
                    } else {
                        tpu::cmd::edit::parse_line_range(range_s)
                            .map_err(|e| format!("splice: {e}"))?
                    };
                    let data = if let Some(ref fmt) = data_format {
                        tpu::data_format::decode(fmt, data_s)
                            .map_err(|e| format!("splice data decode: {e}"))?
                    } else {
                        data_s.as_bytes().to_vec()
                    };
                    // Normalize line endings to LF in text mode; binary data
                    // must not be altered.
                    let data = if binary {
                        data
                    } else {
                        normalize_bytes_to_lf(data)
                    };
                    ops.push(tpu::cmd::edit::EditOp::Splice { start, end, data });
                }
                other => return Err(format!("unknown op: {other:?}").into()),
            }
        }
    }

    let diff = args.get("diff").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut diff_buf: Vec<u8> = Vec::new();
    let diff_out: Option<&mut dyn std::io::Write> = if diff && !binary {
        Some(&mut diff_buf)
    } else {
        None
    };

    tpu::cmd::edit::run(
        path,
        ops,
        binary,
        le_override,
        diff_out,
        tpu::IoMode::Buffered,
        mojibake_policy_from_args(args),
    )?;
    delete_bak_if_exists(&file);
    let stamp = stamp_and_verify(path, config.verify_delay_ms)?;
    if diff && !diff_buf.is_empty() {
        Ok(String::from_utf8_lossy(&diff_buf).into_owned())
    } else {
        Ok(format!(
            "edited '{}' [mtime={}, size={}]",
            file, stamp.mtime_epoch_ms, stamp.size
        ))
    }
}

fn call_read_file_binary(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let file = resolve_file_arg(args)?;

    let byte_range = match args.get("bytes").and_then(|v| v.as_str()) {
        None => None,
        Some(s) => Some(tpu::cmd::read::parse_bytes_arg(s)?),
    };

    let hashes: Vec<String> = args
        .get("hash")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    let hash_specs: Vec<tpu::cmd::read::HashSpec> = hashes
        .iter()
        .map(|s| tpu::cmd::read::parse_hash_arg(s))
        .collect::<Result<Vec<_>, _>>()?;

    let all_bytes = std::fs::read(&file)?;
    let computed_hashes = tpu::cmd::read::compute_hashes(&all_bytes, &hash_specs)?;

    let slice: &[u8] = match byte_range {
        None => &all_bytes[..],
        Some((lo, hi)) => {
            let lo = (lo.saturating_sub(1) as usize).min(all_bytes.len());
            let hi = (hi as usize).min(all_bytes.len());
            &all_bytes[lo..hi]
        }
    };

    if !hash_specs.is_empty() {
        // Return JSON with base64 content and hashes array.
        let hashes_json: Vec<serde_json::Value> = computed_hashes
            .iter()
            .map(|h| {
                serde_json::json!({
                    "algo": match h.algo {
                        tpu::cmd::read::HashAlgo::Crc32 => "crc32",
                        tpu::cmd::read::HashAlgo::Md5 => "md5",
                    },
                    "range": format!("{}-{}", h.start, h.resolved_end),
                    "value": h.hex_value,
                })
            })
            .collect();
        let content = tpu::data_format::encode_base64(slice);
        Ok(serde_json::to_string(&serde_json::json!({
            "reason": "data",
            "subcommand": "read",
            "encoding": "bytes-base64",
            "content": content,
            "hashes": hashes_json,
        }))?)
    } else {
        // Return 7-bit-clean escaped string.
        Ok(tpu::escape::encode_bytes(slice))
    }
}

fn call_read_file_escaped(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let file = resolve_file_arg(args)?;
    let path = std::path::Path::new(&file);

    let lines_range = match args.get("lines").and_then(|v| v.as_str()) {
        None => None,
        Some(s) => Some(tpu::cmd::read::parse_lines_arg(s)?),
    };
    let numbers = args
        .get("numbers")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut buf: Vec<u8> = Vec::new();
    tpu::cmd::readex::run(
        path,
        lines_range,
        numbers,
        tpu::encoding::OutputEncoding::Preserve,
        tpu::encoding::BomPolicy::default(),
        &mut buf,
        tpu::IoMode::Buffered,
        None,
    )?;
    Ok(String::from_utf8(buf).map_err(|e| format!("readex: non-UTF-8 output: {e}"))?)
}

fn call_read_head(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let file = resolve_file_arg(args)?;
    let path = std::path::Path::new(&file);

    let mode = if let Some(n) = args.get("bytes").and_then(|v| v.as_u64()) {
        tpu::cmd::head::HeadMode::Bytes { n }
    } else {
        let n = args.get("lines").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        let numbers = args
            .get("numbers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        tpu::cmd::head::HeadMode::Lines { n, numbers }
    };

    let mut buf: Vec<u8> = Vec::new();
    tpu::cmd::head::run(path, mode, &mut buf, tpu::IoMode::Buffered, None)?;
    Ok(String::from_utf8(buf).map_err(|e| format!("head: non-UTF-8 output: {e}"))?)
}

fn call_read_tail(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let file = resolve_file_arg(args)?;
    let path = std::path::Path::new(&file);

    let mode = if let Some(n) = args.get("bytes").and_then(|v| v.as_u64()) {
        tpu::cmd::tail::TailMode::Bytes { n }
    } else {
        let n = args.get("lines").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        let numbers = args
            .get("numbers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        tpu::cmd::tail::TailMode::Lines { n, numbers }
    };

    let mut buf: Vec<u8> = Vec::new();
    tpu::cmd::tail::run(path, mode, &mut buf, tpu::IoMode::Buffered, None)?;
    Ok(String::from_utf8(buf).map_err(|e| format!("tail: non-UTF-8 output: {e}"))?)
}

fn call_count_file(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let file = resolve_file_arg(args)?;
    let path = std::path::Path::new(&file);

    let lines = args.get("lines").and_then(|v| v.as_bool()).unwrap_or(false);
    let words = args.get("words").and_then(|v| v.as_bool()).unwrap_or(false);
    let chars = args.get("chars").and_then(|v| v.as_bool()).unwrap_or(false);
    let bytes = args.get("bytes").and_then(|v| v.as_bool()).unwrap_or(false);
    let stats = args.get("stats").and_then(|v| v.as_bool()).unwrap_or(false);

    let mut patterns: Vec<String> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    if let Some(entries) = args.get("patterns").and_then(|v| v.as_array()) {
        for entry in entries {
            let pattern = entry
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or("patterns entry missing 'pattern' field")?;
            patterns.push(pattern.to_owned());
            if let Some(label) = entry.get("label").and_then(|v| v.as_str()) {
                labels.push(label.to_owned());
            }
        }
    }

    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let writer = SharedBufWriter(buf.clone());
    let mut out = tpu::output::human_output_to(Box::new(writer));

    tpu::cmd::count::run(
        path,
        lines,
        words,
        chars,
        bytes,
        &patterns,
        &labels,
        stats,
        out.as_mut(),
        tpu::IoMode::Buffered,
    )?;
    drop(out);
    let data = buf.lock().unwrap().clone();
    Ok(String::from_utf8(data).map_err(|e| format!("count: non-UTF-8 output: {e}"))?)
}

fn call_append_file(
    args: &Value,
    config: &ServerConfig,
) -> Result<String, Box<dyn std::error::Error>> {
    let file = resolve_file_arg(args)?;
    let content_raw = require_str(args, "content")?;
    let content = normalize_to_lf(content_raw);
    let path = std::path::Path::new(&file);

    let le_override = match args.get("line_ending").and_then(|v| v.as_str()) {
        None => None,
        Some(s) => Some(tpu::encoding::parse_line_ending(s)?),
    };
    let diff = args.get("diff").and_then(|v| v.as_bool()).unwrap_or(false);

    // Run validate guards before any modification.
    if let Some(validates) = args.get("validate").and_then(|v| v.as_array()) {
        let pairs = flatten_validate_pairs(validates)?;
        tpu::cmd::validate::run_all(&pairs, path, false, tpu::IoMode::Buffered)?;
    }

    if diff {
        let mut diff_buf: Vec<u8> = Vec::new();
        tpu::cmd::append::run(
            path,
            &content,
            le_override,
            Some(&mut diff_buf),
            tpu::IoMode::Buffered,
            mojibake_policy_from_args(args),
        )?;
        if !diff_buf.is_empty() {
            // Skip stamp_and_verify in diff mode to avoid file mutation
            return Ok(String::from_utf8_lossy(&diff_buf).into_owned());
        }
        return Ok(format!("appended to '{file}' (no changes)"));
    }

    tpu::cmd::append::run(path, &content, le_override, None, tpu::IoMode::Buffered, mojibake_policy_from_args(args))?;
    delete_bak_if_exists(&file);
    let stamp = stamp_and_verify(path, config.verify_delay_ms)?;
    Ok(format!(
        "appended to '{}' [mtime={}, size={}]",
        file, stamp.mtime_epoch_ms, stamp.size
    ))
}

fn call_find(args: &Value, config: &ServerConfig) -> Result<String, Box<dyn std::error::Error>> {
    // Collect patterns: primary "pattern" + optional "patterns" array.
    let mut all_patterns: Vec<String> = Vec::new();
    if let Some(p) = args.get("pattern").and_then(|v| v.as_str()) {
        all_patterns.push(p.to_owned());
    }
    if let Some(arr) = args.get("patterns").and_then(|v| v.as_array()) {
        for p in arr {
            if let Some(s) = p.as_str() {
                all_patterns.push(s.to_owned());
            }
        }
    }

    // Collect paths: primary "path" + optional "paths" array; normalise URIs.
    let mut all_paths: Vec<String> = Vec::new();
    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
        all_paths.push(normalize_file_path(p));
    }
    if let Some(arr) = args.get("paths").and_then(|v| v.as_array()) {
        for p in arr {
            if let Some(s) = p.as_str() {
                all_paths.push(normalize_file_path(s));
            }
        }
    }

    if all_patterns.is_empty() {
        return Err("find: at least one pattern is required".into());
    }
    if all_paths.is_empty() {
        return Err("find: at least one path is required".into());
    }

    let fixed_strings = args
        .get("fixed_strings")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let multiline = args
        .get("multiline")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let ignore_case = args
        .get("ignore_case")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let all_match = args
        .get("all_match")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let invert = args
        .get("invert")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let numbers = args
        .get("numbers")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let count = args.get("count").and_then(|v| v.as_bool()).unwrap_or(false);
    let after = args.get("after").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let before = args.get("before").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

    let pattern_refs: Vec<&str> = all_patterns.iter().map(String::as_str).collect();
    let path_refs: Vec<&str> = all_paths.iter().map(String::as_str).collect();

    let on_error = match args.get("on_error").and_then(|v| v.as_str()) {
        Some("fail") => tpu::cmd::copy::OnError::Fail,
        Some("warn") => tpu::cmd::copy::OnError::Warn,
        _ => config.default_on_error,
    };
    let mut walk_warnings: Vec<String> = Vec::new();

    let mut buf: Vec<u8> = Vec::new();
    let result = tpu::cmd::find::run_with_policy(
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
        tpu::IoMode::Buffered,
        on_error,
        &mut walk_warnings,
    );

    match result {
        Ok(_) => {
            let mut text = String::from_utf8(buf)
                .map_err(|e| format!("find: non-UTF-8 output: {e}"))?;
            // Surface walk warnings so Copilot sees a structured note about
            // skipped paths, not silent loss. In summary mode we collapse
            // the per-entry detail into a single tail line.
            if !walk_warnings.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                match config.progress_detail {
                    ProgressDetail::EachFile => {
                        for w in &walk_warnings {
                            text.push_str("warning: ");
                            text.push_str(w);
                            text.push('\n');
                        }
                    }
                    ProgressDetail::Summary => {
                        let n = walk_warnings.len();
                        // Free the strings immediately — in summary mode only
                        // the count is needed, not the individual messages.
                        walk_warnings.clear();
                        walk_warnings.shrink_to_fit();
                        text.push_str(&format!(
                            "warning: {n} path(s) skipped (use progressDetail=each-file to list)\n",
                        ));
                    }
                }
            }
            Ok(text)
        }
        Err(e) => Err(e),
    }
}

fn call_copy_file(
    args: &Value,
    config: &ServerConfig,
) -> Result<String, Box<dyn std::error::Error>> {
    let source = normalize_file_path(require_str(args, "source")?);
    let dest = normalize_file_path(require_str(args, "dest")?);
    let recursive = args.get("recursive").and_then(|v| v.as_bool()).unwrap_or(false);
    let overwrite = args.get("overwrite").and_then(|v| v.as_bool()).unwrap_or(false);
    let on_error = match args.get("on_error").and_then(|v| v.as_str()) {
        Some("fail") => tpu::cmd::copy::OnError::Fail,
        Some("warn") => tpu::cmd::copy::OnError::Warn,
        _ => config.default_on_error,
    };
    let opts = tpu::cmd::copy::CopyOptions { recursive, overwrite, on_error };

    // In EachFile mode, capture Shell::warn output for the `log` array.  In
    // Summary mode the warning count is already in report.warnings, so we
    // write to a sink to avoid retaining every warning string in memory for
    // large tree walks.
    let warn_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    struct SharedWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for SharedWriter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut shell = if matches!(config.progress_detail, ProgressDetail::EachFile) {
        tpu::shell::Shell::from_write(Box::new(SharedWriter(warn_buf.clone())))
    } else {
        tpu::shell::Shell::from_write(Box::new(std::io::sink()))
    };

    let report = tpu::cmd::copy::run(&source, std::path::Path::new(&dest), opts, &mut shell)?;
    drop(shell);

    // Human-mode warn() emits "warning: {message}\n"; strip the prefix to get
    // a clean string array that MCP clients can read directly.
    let raw = String::from_utf8_lossy(&warn_buf.lock().unwrap()).into_owned();
    let warn_lines: Vec<&str> = raw
        .lines()
        .map(|l| l.strip_prefix("warning: ").unwrap_or(l))
        .filter(|l| !l.is_empty())
        .collect();
    let mut result = serde_json::json!({
        "copied":   report.copied,
        "skipped":  report.skipped,
        "warnings": report.warnings,
    });
    if matches!(config.progress_detail, ProgressDetail::EachFile) {
        result["log"] = serde_json::Value::Array(
            warn_lines.iter().map(|s| serde_json::Value::String((*s).to_string())).collect(),
        );
    }
    Ok(serde_json::to_string(&result)?)
}

fn call_render_file(
    args: &Value,
    config: &ServerConfig,
) -> Result<String, Box<dyn std::error::Error>> {
    use std::collections::BTreeMap;
    let output = normalize_file_path(require_str(args, "output")?);
    // Normalize CRLF → LF at the MCP boundary, consistent with other write tools.
    let template_inline_owned = args
        .get("template")
        .and_then(|v| v.as_str())
        .map(|s| s.replace("\r\n", "\n").replace('\r', "\n"));
    let template_inline = template_inline_owned.as_deref();
    let template_file = args.get("template_file").and_then(|v| v.as_str()).map(normalize_file_path);
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    if let Some(map) = args.get("vars").and_then(|v| v.as_object()) {
        for (k, v) in map {
            // Enforce the same key constraints as the CLI parser.
            if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                return Err(format!(
                    "render: vars key {k:?} may only contain ASCII letters, digits, '_' or '-'"
                )
                .into());
            }
            let val = v.as_str().ok_or_else(|| {
                format!("render: vars[{k}]: value must be a string")
            })?;
            // Normalize CRLF → LF so variable values don't produce doubled
            // carriage returns when written to CRLF-convention destination files.
            vars.insert(k.clone(), val.replace("\r\n", "\n").replace('\r', "\n"));
        }
    }
    let missing = match args.get("missing").and_then(|v| v.as_str()) {
        Some("empty") => tpu::cmd::render::MissingPolicy::Empty,
        Some("leave") => tpu::cmd::render::MissingPolicy::Leave,
        _ => tpu::cmd::render::MissingPolicy::Error,
    };
    let policy = mojibake_policy_from_args(args);
    let report = tpu::cmd::render::run(
        std::path::Path::new(&output),
        template_inline,
        template_file.as_deref().map(std::path::Path::new),
        None,
        &vars,
        missing,
        tpu::IoMode::Buffered,
        policy,
    )?;
    let stamp = stamp_and_verify(std::path::Path::new(&output), config.verify_delay_ms)?;
    delete_bak_if_exists(&output);
    Ok(serde_json::to_string(&serde_json::json!({
        "output": output,
        "substitutions": report.substitutions,
        "missing": report.missing,
        "mtime_epoch_ms": stamp.mtime_epoch_ms,
        "size": stamp.size,
    }))?)
}

fn call_setup(
    args: &Value,
    config: &ServerConfig,
) -> Result<String, Box<dyn std::error::Error>> {
    let target = args
        .get("target")
        .and_then(|v| v.as_str())
        .map(normalize_file_path);
    match target {
        None => Ok(tpu::cmd::setup::full_block()),
        Some(path) => {
            let (updated, replaced) =
                tpu::cmd::setup::inject(std::path::Path::new(&path), tpu::IoMode::Buffered)?;
            let mut result = serde_json::json!({
                "target": path,
                "updated": updated,
                "replaced": replaced,
            });
            if updated {
                let stamp =
                    stamp_and_verify(std::path::Path::new(&path), config.verify_delay_ms)?;
                delete_bak_if_exists(&path);
                result["mtime_epoch_ms"] =
                    serde_json::Value::Number(stamp.mtime_epoch_ms.into());
                result["size"] = serde_json::Value::Number(stamp.size.into());
            }
            Ok(serde_json::to_string(&result)?)
        }
    }
}

fn call_stat_file(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let file = resolve_file_arg(args)?;
    let meta = std::fs::metadata(&file)?;
    let mtime_epoch_ms = mtime_as_epoch_ms(&meta);
    let created_epoch_ms = meta
        .created()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let readonly = meta.permissions().readonly();
    let size = meta.len();
    Ok(serde_json::to_string(&serde_json::json!({
        "size": size,
        "mtime_epoch_ms": mtime_epoch_ms,
        "created_epoch_ms": created_epoch_ms,
        "readonly": readonly,
    }))?)
}

fn call_validate_file(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    // `tpu` has no standalone `validate` subcommand; --validate is a pre-write
    // flag on `tpu write`.  This tool calls the library directly to avoid an
    // unnecessary write operation.
    let file = resolve_file_arg(args)?;
    let selector = require_str(args, "selector")?;
    let value = require_str(args, "value")?;

    let is_binary = args
        .get("is_binary")
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| is_binary_selector(selector));

    let pairs = vec![selector.to_string(), value.to_string()];
    tpu::cmd::validate::run_all(&pairs, Path::new(&file), is_binary, tpu::IoMode::Buffered)?;
    Ok("validation passed".into())
}

// -- helpers -------------------------------------------------------------------

/// Delete `<file>.bak` if it exists. Ignores all errors — the .bak is
/// advisory and its absence is never a failure.
/// Server-level configuration parsed from `tpu-mcp` command-line arguments.
///
/// Pass a reference to [`call`] on every tool invocation so that machine-level
/// settings (such as the Windows Defender write-verification delay) are applied
/// uniformly without Copilot needing to supply per-call parameters.
#[derive(Clone)]
pub struct ServerConfig {
    /// Milliseconds to wait after a mutating file operation before reading
    /// back metadata to verify the write persisted.
    ///
    /// Default: 100.  Windows Defender's minifilter may silently revert a
    /// write asynchronously; a short pause gives it time to act so the
    /// mismatch can be caught and reported.
    ///
    /// Set to 0 to skip the stamp-and-verify cycle entirely, e.g. when a
    /// Defender exclusion has been configured for the binary or workspace.
    pub verify_delay_ms: u64,

    /// When true, emit a one-line `dispatched '<method>'` log notification
    /// for every JSON-RPC request. On by default; suppress with `--quiet` or
    /// `TPU_MCP_QUIET=1`. Trace lines are sent via MCP `notifications/message`
    /// (level `info`), not stderr, so they appear in the client's MCP output
    /// channel as informational rather than warnings.
    pub trace: bool,

    /// Default policy for tools that walk file trees (`tpu_find`,
    /// `tpu_copy_file`) when an entry cannot be read or written.
    ///
    /// `Warn` (default) emits a streamed warning record and continues with
    /// the next entry. `Fail` aborts the operation on the first error.
    /// Per-call `on_error` arguments override this default.
    pub default_on_error: tpu::cmd::copy::OnError,

    /// How verbose the per-call result of tree-walking tools should be.
    ///
    /// `EachFile` (default) returns the per-entry warning log (inaccessible
    /// paths and other walk errors) along with the summary counts. `Summary`
    /// suppresses the per-entry warning log and returns only the aggregate
    /// counts. (Note: `tpu_find` additionally appends a single tail line
    /// with the warning count in summary mode; other tree-walking tools
    /// return only the numeric count in the JSON result.)
    /// Useful for clients that want a quieter trail in the MCP output channel.
    pub progress_detail: ProgressDetail,
}

/// How much per-entry detail tree-walking tools should include in their
/// JSON tool result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProgressDetail {
    /// Include the `log` of per-entry warnings (inaccessible paths and walk errors).
    #[default]
    EachFile,
    /// Suppress the per-entry log; emit only summary counts.
    Summary,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            verify_delay_ms: 100,
            trace: true,
            default_on_error: tpu::cmd::copy::OnError::Warn,
            progress_detail: ProgressDetail::EachFile,
        }
    }
}

/// Metadata written and immediately read back after a successful file mutation.
struct WriteStamp {
    mtime_epoch_ms: u64,
    size: u64,
}

/// Stamp a file's mtime to a known value, wait `delay_ms` milliseconds for
/// Windows Defender's minifilter to act, then read back the metadata to verify
/// the write was not silently reverted.
///
/// When `delay_ms` is 0, skips the stamp-and-verify cycle; metadata is read
/// and returned immediately without modifying the mtime.
///
/// Returns an error if the read-back mtime diverges from the stamped value by
/// more than 10 ms, identifying Defender as the likely cause.
fn stamp_and_verify(file: &Path, delay_ms: u64) -> Result<WriteStamp, Box<dyn std::error::Error>> {
    if delay_ms == 0 {
        let meta = std::fs::metadata(file)?;
        return Ok(WriteStamp {
            mtime_epoch_ms: mtime_as_epoch_ms(&meta),
            size: meta.len(),
        });
    }

    // Set mtime to a known millisecond-precision stamp.
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_millis() as u64;
    let stamp_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(now_ms);
    {
        let f = std::fs::OpenOptions::new().write(true).open(file)?;
        f.set_times(std::fs::FileTimes::new().set_modified(stamp_time))?;
    }

    // Give Defender's minifilter time to act.
    std::thread::sleep(std::time::Duration::from_millis(delay_ms));

    // Read back and verify.
    let meta = std::fs::metadata(file)?;
    let actual_ms = mtime_as_epoch_ms(&meta);

    if actual_ms.abs_diff(now_ms) > 10 {
        return Err(format!(
            "write verification failed for '{}': mtime stamp was {now_ms} ms but \
             read back {actual_ms} ms -- this likely indicates Windows Defender \
             interference.  Add a Defender exclusion for the tpu-mcp binary or the \
             workspace directory, or pass --verify-delay-ms=0 to tpu-mcp to disable \
             verification.",
            file.display(),
        )
        .into());
    }

    Ok(WriteStamp {
        mtime_epoch_ms: actual_ms,
        size: meta.len(),
    })
}

/// Extract the last-modified time from metadata as milliseconds since the
/// Unix epoch.  Returns 0 when the platform does not support mtime.
fn mtime_as_epoch_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn delete_bak_if_exists(file: &str) {
    let _ = std::fs::remove_file(format!("{file}.bak"));
}

/// Normalize all line endings in `s` to LF (`\n`).
///
/// Replaces CRLF (`\r\n`) and bare CR (`\r`) with LF.  Pure-LF input is
/// returned without allocation (the `Cow` borrows the original `&str`).
///
/// This is the MCP boundary normalization gate: every text value received
/// from a Copilot/MCP client passes through this function before being
/// handed to any `tpu::cmd::*` library function, which all expect LF-only
/// input.  Without this, stray CRLF in JSON strings would produce `\r\r\n`
/// on CRLF-target files or inject CRLF into LF-target files.
fn normalize_to_lf(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('\r') {
        // Fast path -- no CR bytes at all; nothing to do.
        std::borrow::Cow::Borrowed(s)
    } else {
        // Replace CRLF first (greedy), then any remaining bare CR.
        std::borrow::Cow::Owned(s.replace("\r\n", "\n").replace('\r', "\n"))
    }
}

/// Expand C-style backslash escape sequences in a regex replacement template.
///
/// Recognised sequences and their expansions:
///   - `\n`  ? LF   (`0x0A`)
///   - `\r`  ? CR   (`0x0D`)
///   - `\t`  ? TAB  (`0x09`)
///   - `\\` ? `\`
///
/// All other `\X` sequences are passed through unchanged � both the backslash
/// and the following character are preserved as-is.  This is intentional:
/// `$1`, `$name`, and `$$` reference syntax used by `regex::bytes::Captures::
/// expand()` must reach the regex engine unaltered.
///
/// A trailing backslash with no following character is also preserved
/// unchanged.
///
/// After unescape, the caller should run `normalize_to_lf` so that any `\r`
/// or `\r\n` sequences introduced by `\r`/`\n` unescape are folded to LF
/// before the normalised tpu view sees them.
fn unescape_replacement(s: &str) -> String {
    if !s.contains('\\') {
        // Fast path � no backslashes at all; nothing to do.
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'), // trailing backslash: preserve
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Normalize all line endings in a byte slice to LF (`\n`).
///
/// Same semantics as [`normalize_to_lf`] but operates on raw bytes.  Used
/// for edit-op data that is already extracted as `Vec<u8>`.
fn normalize_bytes_to_lf(bytes: Vec<u8>) -> Vec<u8> {
    if !bytes.contains(&b'\r') {
        return bytes;
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            out.push(b'\n');
            // Skip the \n in a \r\n pair.
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 1;
            }
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    out
}

/// Flatten an array of `{"selector": "...", "value": "..."}` objects into a
/// flat `Vec<String>` of alternating `[selector, value, ...]` pairs, as
/// expected by `tpu::cmd::validate::run_all`.
fn flatten_validate_pairs(validates: &[Value]) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut pairs = Vec::with_capacity(validates.len() * 2);
    for v in validates {
        let sel = v
            .get("selector")
            .and_then(|s| s.as_str())
            .ok_or("validate entry missing 'selector'")?;
        let val = v
            .get("value")
            .and_then(|s| s.as_str())
            .ok_or("validate entry missing 'value'")?;
        pairs.push(sel.to_owned());
        pairs.push(val.to_owned());
    }
    Ok(pairs)
}

/// Parse a data-format name into a [`tpu::data_format::DataFormat`] value.
fn parse_data_format(s: &str) -> Result<tpu::data_format::DataFormat, Box<dyn std::error::Error>> {
    match s {
        "hex" => Ok(tpu::data_format::DataFormat::Hex),
        "base64" => Ok(tpu::data_format::DataFormat::Base64),
        "encoded" => Ok(tpu::data_format::DataFormat::Encoded),
        other => Err(format!(
            "unrecognised data_format value {other:?}; expected hex, base64, or encoded"
        )
        .into()),
    }
}

/// A `Write` adapter backed by an `Arc<Mutex<Vec<u8>>>` so that the buffer
/// contents remain accessible after the writer (and the `Output` wrapping
/// it) are dropped.
struct SharedBufWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for SharedBufWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// SAFETY: The MCP server is single-threaded; Arc<Mutex<_>> is used solely to
// allow shared ownership across the Output trait boundary.
unsafe impl Send for SharedBufWriter {}

fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, Box<dyn std::error::Error>> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing required argument '{key}'").into())
}

/// Build a [`tpu::mojibake::WritePolicy`] from a tool-call's JSON args.
///
/// Recognises the `allow_mojibake` boolean (default `false`); when `true`,
/// the write-time mojibake guard is disabled for that call.  Mirrors the
/// CLI's `--allow-mojibake` flag.
fn mojibake_policy_from_args(args: &Value) -> tpu::mojibake::WritePolicy {
    let allow = args
        .get("allow_mojibake")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if allow {
        tpu::mojibake::WritePolicy::permissive()
    } else {
        tpu::mojibake::WritePolicy::default()
    }
}

/// Extract the `file` argument and normalise it to an OS path string.
///
/// Accepts both plain OS paths and `file://` URIs.  VS Code Copilot passes
/// `file:///` URIs when it calls MCP tools during operations such as
/// "review changes", so we strip the scheme here rather than letting tpu
/// receive an un-openable URI string.
///
/// Normalisation steps:
///   1. Strip `file://` (and the always-empty authority).  On Windows,
///      `file:///C:/foo` → `C:/foo`; on Unix, `file:///foo` → `/foo`.
///   2. Minimal percent-decoding: `%3A` → `:`, `%2F` → `/`, `%5C` → `\`,
///      and all other `%XX` sequences.  Windows drive-letter colons are
///      sometimes percent-encoded by strict URI serialisers.
fn resolve_file_arg(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let raw = require_str(args, "file")?;
    Ok(normalize_file_path(raw))
}

fn normalize_file_path(raw: &str) -> String {
    let without_scheme = if let Some(rest) = raw.strip_prefix("file://") {
        // After "file://" the spec requires an (always-empty for local files)
        // authority followed by the absolute path.  Strip one leading slash
        // that is part of the authority/path separator for the common forms:
        //   "file:///C:/foo" → "C:/foo"   (Windows — drop the extra slash)
        //   "file:///foo"    → "/foo"      (Unix   — keep the slash)
        if let Some(after_slash) = rest.strip_prefix('/') {
            if is_windows_drive_path(after_slash) {
                after_slash
            } else {
                rest // preserve the single leading slash for Unix paths
            }
        } else {
            rest
        }
    } else {
        raw
    };

    percent_decode_path(without_scheme)
}

/// Return true if `s` starts with a Windows drive specifier such as `C:/`,
/// `c:\`, or the percent-encoded form `C%3A/`.
fn is_windows_drive_path(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 3 {
        return false;
    }
    let drive = b[0];
    if !drive.is_ascii_alphabetic() {
        return false;
    }
    // "C:/" or "C:\"
    if b[1] == b':' && (b[2] == b'/' || b[2] == b'\\') {
        return true;
    }
    // "C%3A/" or "C%3A\" (case-insensitive on the hex digits)
    if b.len() >= 6
        && b[1] == b'%'
        && b[2].eq_ignore_ascii_case(&b'3')
        && b[3].eq_ignore_ascii_case(&b'A')
        && (b[4] == b'/' || b[4] == b'\\')
    {
        return true;
    }
    false
}

/// Decode all `%XX` percent-encoded sequences in `s`.
///
/// Invalid or incomplete sequences are passed through unchanged.
fn percent_decode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len()
            && let (Some(hi), Some(lo)) = (hex_nibble(b[i + 1]), hex_nibble(b[i + 2])) {
                out.push(char::from(hi << 4 | lo));
                i += 3;
                continue;
            }
        out.push(char::from(b[i]));
        i += 1;
    }
    out
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Return true when `selector` prefix indicates a binary-mode validator.
fn is_binary_selector(selector: &str) -> bool {
    selector.starts_with("bytes:") || selector.starts_with("md5:") || selector.starts_with("crc32:")
}

// -- unit tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn np(s: &str) -> String {
        normalize_file_path(s)
    }

    /// Guard: `TOOL_NAMES` must stay in sync with the `name` fields embedded
    /// in [`list()`]. If a tool is added to `list()` without updating
    /// `TOOL_NAMES` (or vice versa) this test catches it.
    #[test]
    fn tool_names_match_list_payload() {
        let from_list: Vec<String> = list()
            .as_array()
            .expect("list() returns an array")
            .iter()
            .map(|t| {
                t.get("name")
                    .and_then(|n| n.as_str())
                    .expect("each tool has a string `name`")
                    .to_owned()
            })
            .collect();
        let from_const: Vec<String> = TOOL_NAMES.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(from_const, from_list, "TOOL_NAMES out of sync with list()");
    }

    // -- file:// URI stripping --

    #[test]
    fn uri_windows_backslash_absolute() {
        assert_eq!(
            np(r"z:\src4\FunE-Tools\README.md"),
            r"z:\src4\FunE-Tools\README.md"
        );
    }

    #[test]
    fn uri_windows_forward_slash_absolute() {
        assert_eq!(
            np("z:/src4/FunE-Tools/README.md"),
            "z:/src4/FunE-Tools/README.md"
        );
    }

    #[test]
    fn uri_file_scheme_windows_uppercase_drive() {
        assert_eq!(
            np("file:///Z:/src4/FunE-Tools/README.md"),
            "Z:/src4/FunE-Tools/README.md"
        );
    }

    #[test]
    fn uri_file_scheme_windows_lowercase_drive() {
        assert_eq!(
            np("file:///z:/src4/FunE-Tools/README.md"),
            "z:/src4/FunE-Tools/README.md"
        );
    }

    #[test]
    fn uri_file_scheme_windows_percent_encoded_colon() {
        assert_eq!(
            np("file:///z%3A/src4/FunE-Tools/README.md"),
            "z:/src4/FunE-Tools/README.md"
        );
    }

    #[test]
    fn uri_file_scheme_windows_percent_encoded_colon_uppercase_hex() {
        assert_eq!(np("file:///Z%3A/src4/foo.rs"), "Z:/src4/foo.rs");
    }

    #[test]
    fn uri_file_scheme_unix_absolute() {
        assert_eq!(np("file:///home/user/foo.txt"), "/home/user/foo.txt");
    }

    #[test]
    fn uri_file_scheme_two_slashes_unix() {
        // "file://" with only the host portion omitted — unusual but valid input
        assert_eq!(np("file:///tmp/foo.txt"), "/tmp/foo.txt");
    }

    #[test]
    fn uri_plain_relative_path() {
        assert_eq!(
            np("src/tools/tpu-mcp/README.md"),
            "src/tools/tpu-mcp/README.md"
        );
    }

    #[test]
    fn uri_no_scheme_unchanged() {
        assert_eq!(np("/absolute/unix/path.txt"), "/absolute/unix/path.txt");
    }

    // -- percent-decoding --

    #[test]
    fn percent_decode_colon() {
        assert_eq!(percent_decode_path("z%3A/foo"), "z:/foo");
    }

    #[test]
    fn percent_decode_uppercase_hex() {
        assert_eq!(percent_decode_path("z%3A/foo%2Fbar"), "z:/foo/bar");
    }

    #[test]
    fn percent_decode_space() {
        assert_eq!(percent_decode_path("my%20file.txt"), "my file.txt");
    }

    #[test]
    fn percent_decode_invalid_sequence_passthrough() {
        // '%GG' is not valid hex — pass both chars through unchanged
        assert_eq!(percent_decode_path("foo%GGbar"), "foo%GGbar");
    }

    #[test]
    fn percent_decode_truncated_sequence_passthrough() {
        // '%4' at end of string — incomplete, pass through
        assert_eq!(percent_decode_path("foo%4"), "foo%4");
    }

    // -- is_windows_drive_path --

    #[test]
    fn drive_path_detects_colon_slash() {
        assert!(is_windows_drive_path("C:/foo"));
        assert!(is_windows_drive_path("z:/foo"));
        assert!(is_windows_drive_path(r"C:\foo"));
    }

    #[test]
    fn drive_path_detects_percent_encoded() {
        assert!(is_windows_drive_path("C%3A/foo"));
        assert!(is_windows_drive_path("c%3a/foo"));
    }

    #[test]
    fn drive_path_rejects_unix_paths() {
        assert!(!is_windows_drive_path("/home/user"));
        assert!(!is_windows_drive_path("foo/bar"));
        assert!(!is_windows_drive_path(""));
    }

    // -- normalize_to_lf --

    #[test]
    fn normalize_lf_only_borrows() {
        let input = "line1\nline2\nline3\n";
        let result = normalize_to_lf(input);
        assert!(matches!(result, std::borrow::Cow::Borrowed(_)));
        assert_eq!(&*result, input);
    }

    #[test]
    fn normalize_crlf_to_lf() {
        assert_eq!(&*normalize_to_lf("a\r\nb\r\nc\r\n"), "a\nb\nc\n");
    }

    #[test]
    fn normalize_bare_cr_to_lf() {
        assert_eq!(&*normalize_to_lf("a\rb\rc\r"), "a\nb\nc\n");
    }

    #[test]
    fn normalize_mixed_endings_to_lf() {
        // Mix of CRLF, bare CR, and LF
        assert_eq!(&*normalize_to_lf("a\r\nb\rc\nd\r\n"), "a\nb\nc\nd\n");
    }

    #[test]
    fn normalize_empty_string() {
        assert_eq!(&*normalize_to_lf(""), "");
    }

    #[test]
    fn normalize_no_newlines() {
        assert_eq!(&*normalize_to_lf("hello world"), "hello world");
    }

    #[test]
    fn normalize_consecutive_crlf() {
        assert_eq!(&*normalize_to_lf("\r\n\r\n"), "\n\n");
    }

    #[test]
    fn normalize_cr_lf_cr_sequence() {
        // \r followed by \n followed by \r -- the \r\n is a pair, then bare \r
        assert_eq!(&*normalize_to_lf("a\r\n\rb"), "a\n\nb");
    }

    #[test]
    fn normalize_trailing_cr() {
        assert_eq!(&*normalize_to_lf("abc\r"), "abc\n");
    }

    #[test]
    fn normalize_leading_crlf() {
        assert_eq!(&*normalize_to_lf("\r\nabc"), "\nabc");
    }

    // -- unescape_replacement --

    fn ur(s: &str) -> String {
        unescape_replacement(s)
    }

    #[test]
    fn unescape_empty_string() {
        assert_eq!(ur(""), "");
    }

    #[test]
    fn unescape_no_backslashes_fast_path() {
        // No backslash at all � fast path returns owned copy unchanged.
        let input = "hello $1 world";
        assert_eq!(ur(input), input);
    }

    #[test]
    fn unescape_newline_sequence() {
        assert_eq!(ur("\\n"), "\n");
    }

    #[test]
    fn unescape_tab_sequence() {
        assert_eq!(ur("\\t"), "\t");
    }

    #[test]
    fn unescape_cr_sequence() {
        assert_eq!(ur("\\r"), "\r");
    }

    #[test]
    fn unescape_double_backslash_gives_single() {
        assert_eq!(ur("\\\\"), "\\");
    }

    #[test]
    fn unescape_embedded_newline_in_text() {
        assert_eq!(ur("hello\\nworld"), "hello\nworld");
    }

    #[test]
    fn unescape_multiple_sequences() {
        assert_eq!(ur("\\n\\t\\r\\\\"), "\n\t\r\\");
    }

    #[test]
    fn unescape_capture_group_passthrough() {
        // $1 and $name must survive unmodified � no backslash involved.
        assert_eq!(ur("prefix $1 suffix"), "prefix $1 suffix");
        assert_eq!(ur("$name"), "$name");
        assert_eq!(ur("$$"), "$$");
    }

    #[test]
    fn unescape_capture_group_mixed_with_newline() {
        assert_eq!(ur("$1\\n$2"), "$1\n$2");
    }

    #[test]
    fn unescape_double_backslash_n_gives_backslash_n() {
        // \\n should expand to literal backslash + n, NOT a newline.
        // (The \\ is consumed first as a single \, then n is the next char.)
        assert_eq!(ur("\\\\n"), "\\n");
    }

    #[test]
    fn unescape_trailing_backslash_preserved() {
        // A bare trailing backslash with nothing following it is passed through.
        assert_eq!(ur("hello\\"), "hello\\");
    }

    #[test]
    fn unescape_unknown_escape_preserved() {
        // \x, \$, \q etc. are not recognised � both chars pass through.
        assert_eq!(ur("\\x41"), "\\x41");
        assert_eq!(ur("\\$1"), "\\$1");
        assert_eq!(ur("\\q"), "\\q");
    }

    #[test]
    fn unescape_consecutive_newlines() {
        assert_eq!(ur("\\n\\n\\n"), "\n\n\n");
    }

    #[test]
    fn unescape_newline_at_start_and_end() {
        assert_eq!(ur("\\nfoo\\n"), "\nfoo\n");
    }

    #[test]
    fn unescape_no_change_when_only_dollar() {
        // Dollar signs are regex capture-group syntax; unescape must not
        // alter them even when backslashes also appear elsewhere.
        assert_eq!(ur("line1\\nline2 $$ literal"), "line1\nline2 $$ literal");
    }

    // -- normalize_bytes_to_lf --

    #[test]
    fn normalize_bytes_lf_only_no_copy() {
        let input = b"line1\nline2\n".to_vec();
        let result = normalize_bytes_to_lf(input.clone());
        assert_eq!(result, input);
    }

    #[test]
    fn normalize_bytes_crlf() {
        assert_eq!(normalize_bytes_to_lf(b"a\r\nb\r\n".to_vec()), b"a\nb\n");
    }

    #[test]
    fn normalize_bytes_bare_cr() {
        assert_eq!(normalize_bytes_to_lf(b"a\rb\r".to_vec()), b"a\nb\n");
    }

    #[test]
    fn normalize_bytes_mixed() {
        assert_eq!(normalize_bytes_to_lf(b"a\r\nb\rc\n".to_vec()), b"a\nb\nc\n");
    }

    #[test]
    fn normalize_bytes_empty() {
        assert_eq!(normalize_bytes_to_lf(vec![]), Vec::<u8>::new());
    }
}

// -- integration tests ---------------------------------------------------------

#[cfg(test)]
mod integration_tests {
    use std::fs;

    use super::*;

    /// Zero-delay wrapper so existing tests call the real `call()` without
    /// needing to supply a `ServerConfig` argument explicitly.
    fn call(name: &str, args: &serde_json::Value) -> Result<String, Box<dyn std::error::Error>> {
        super::call(
            name,
            args,
            &ServerConfig {
                verify_delay_ms: 0,
                trace: false,
                default_on_error: tpu::cmd::copy::OnError::Warn,
                progress_detail: ProgressDetail::EachFile,
            },
        )
    }

    /// SF-IT-14: `tpu_find` MCP tool integration.
    ///
    /// Calls `tools::call("tpu_find", ...)` directly, exercising the full
    /// tpu-mcp library call path without the JSON-RPC wire layer.
    #[test]
    fn sf_it_14_search_file_mcp_tool() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("search_target.txt");
        // Three lines; lines 1 and 3 contain "fox", line 2 does not.
        fs::write(&f, "alpha fox here\nbeta bar there\ngamma fox again\n").unwrap();

        // tpu_find: pattern + path arguments; returns plain grep-style output.
        let args = serde_json::json!({
            "pattern": "fox",
            "path": f.to_str().expect("temp path must be valid UTF-8"),
        });

        let result =
            call("tpu_find", &args).expect("tpu_find must succeed (exit 0 = matches found)");

        // tpu find emits matching lines, one per LF-terminated line.
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 matching lines; got: {result:?}");
        assert!(
            lines[0].contains("alpha fox here"),
            "line 0 must contain 'alpha fox here'; got: {:?}",
            lines[0]
        );
        assert!(
            lines[1].contains("gamma fox again"),
            "line 1 must contain 'gamma fox again'; got: {:?}",
            lines[1]
        );

        // tpu_find with no matches must return Ok("") -- not an error.
        let args_no_match = serde_json::json!({
            "pattern": "zzz_no_match_zzz",
            "path": f.to_str().expect("temp path must be valid UTF-8"),
        });
        let no_match =
            call("tpu_find", &args_no_match).expect("tpu_find with no matches must be Ok, not Err");
        assert!(
            no_match.trim().is_empty(),
            "no-match result must be empty; got: {no_match:?}"
        );

        drop(dir);
    }

    /// NL-IT-1: `tpu_write_file` normalizes CRLF in content to LF before writing.
    ///
    /// Write a new file with CRLF-containing content.  The written bytes must
    /// contain only LF line endings (the default for a new file).
    #[test]
    fn nl_it_1_write_file_normalizes_crlf() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("write_crlf.txt");

        // Content intentionally has \r\n line endings.
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "content": "line1\r\nline2\r\nline3\r\n",
        });

        call("tpu_write_file", &args).expect("tpu_write_file must succeed");

        let bytes = fs::read(&f).expect("read back written file");
        assert!(
            !bytes.contains(&b'\r'),
            "written file must not contain CR bytes; got: {bytes:?}"
        );
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            "line1\nline2\nline3\n"
        );

        drop(dir);
    }

    /// NL-IT-2: `tpu_write_file` preserves CRLF-target file endings when
    /// the input contains CRLF (which would otherwise produce \r\r\n).
    #[test]
    fn nl_it_2_write_crlf_file_no_double_cr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("crlf_target.txt");

        // Create a CRLF file so tpu detects CRLF as the dominant ending.
        fs::write(&f, "existing\r\ncontent\r\n").unwrap();

        // Overwrite with CRLF-containing content (should be normalized to LF
        // before denormalization to CRLF, avoiding \r\r\n).
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "content": "new\r\ndata\r\n",
        });

        call("tpu_write_file", &args).expect("tpu_write_file must succeed");

        let bytes = fs::read(&f).expect("read back written file");
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains("\r\r"),
            "file must not contain double-CR; got: {text:?}"
        );
        assert_eq!(text, "new\r\ndata\r\n");

        drop(dir);
    }

    /// NL-IT-3: `tpu_replace_in_file` normalizes CRLF in replacement text.
    #[test]
    fn nl_it_3_replace_normalizes_crlf_replacement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("replace_crlf.txt");

        // Create an LF file.
        fs::write(&f, "hello world\n").unwrap();

        // Replacement text has CRLF.
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": "hello world",
            "replacement": "hello\r\nworld",
            "fixed_strings": true,
        });

        call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        let bytes = fs::read(&f).expect("read back replaced file");
        assert!(
            !bytes.contains(&b'\r'),
            "LF file must not contain CR after replace; got: {bytes:?}"
        );
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "hello\nworld\n");

        drop(dir);
    }

    /// NL-IT-4: `tpu_replace_in_file` on a CRLF file with CRLF replacement
    /// produces clean CRLF output, not \r\r\n.
    #[test]
    fn nl_it_4_replace_crlf_file_no_double_cr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("replace_crlf_target.txt");

        // Create a CRLF file.
        fs::write(&f, "alpha beta\r\ngamma\r\n").unwrap();

        // Replacement with CRLF ending.
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": "alpha beta",
            "replacement": "alpha\r\nbeta",
            "fixed_strings": true,
        });

        call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        let bytes = fs::read(&f).expect("read back replaced file");
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains("\r\r"),
            "CRLF file must not contain double-CR; got: {text:?}"
        );
        assert_eq!(text, "alpha\r\nbeta\r\ngamma\r\n");

        drop(dir);
    }

    /// NL-IT-5: `tpu_append_file` normalizes CRLF in appended content.
    #[test]
    fn nl_it_5_append_normalizes_crlf() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("append_crlf.txt");

        // Create an LF file.
        fs::write(&f, "line1\n").unwrap();

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "content": "line2\r\nline3\r\n",
        });

        call("tpu_append_file", &args).expect("tpu_append_file must succeed");

        let bytes = fs::read(&f).expect("read back appended file");
        assert!(
            !bytes.contains(&b'\r'),
            "LF file must not contain CR after append; got: {bytes:?}"
        );
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            "line1\nline2\nline3\n"
        );

        drop(dir);
    }

    /// NL-IT-6: `tpu_edit_file` splice normalizes CRLF in splice data.
    #[test]
    fn nl_it_6_edit_splice_normalizes_crlf() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("edit_crlf.txt");

        // Create an LF file with two lines.
        fs::write(&f, "first\nsecond\n").unwrap();

        // Splice line 1 with CRLF-containing replacement.
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "ops": [
                { "op": "splice", "range": "1-1", "data": "alpha\r\nbeta\r\n" }
            ],
        });

        call("tpu_edit_file", &args).expect("tpu_edit_file must succeed");

        let bytes = fs::read(&f).expect("read back edited file");
        assert!(
            !bytes.contains(&b'\r'),
            "LF file must not contain CR after edit; got: {bytes:?}"
        );
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            "alpha\nbeta\nsecond\n"
        );

        drop(dir);
    }

    /// NL-IT-7: `tpu_edit_file` insert normalizes CRLF in inserted data.
    #[test]
    fn nl_it_7_edit_insert_normalizes_crlf() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("edit_insert_crlf.txt");

        // Create an LF file.
        fs::write(&f, "existing\n").unwrap();

        // Insert at line 1 with CRLF data.
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "ops": [
                { "op": "insert", "offset": "1", "data": "new\r\nlines\r\n" }
            ],
        });

        call("tpu_edit_file", &args).expect("tpu_edit_file must succeed");

        let bytes = fs::read(&f).expect("read back edited file");
        assert!(
            !bytes.contains(&b'\r'),
            "LF file must not contain CR after insert; got: {bytes:?}"
        );

        drop(dir);
    }

    /// NL-IT-8: Mixed line endings in content (CRLF + bare CR + LF) are all
    /// normalized to LF.
    #[test]
    fn nl_it_8_write_mixed_endings_all_normalized() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("mixed_endings.txt");

        // Content has all three styles: CRLF, bare CR, and LF.
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "content": "a\r\nb\rc\nd\r\n",
        });

        call("tpu_write_file", &args).expect("tpu_write_file must succeed");

        let bytes = fs::read(&f).expect("read back written file");
        assert!(
            !bytes.contains(&b'\r'),
            "all CR must be normalized away; got: {bytes:?}"
        );
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "a\nb\nc\nd\n");

        drop(dir);
    }

    /// NL-IT-9: `tpu_replace_in_file` with `--` prefixed replacement text
    /// works correctly (validates the library-call architecture too).
    #[test]
    fn nl_it_9_replace_dash_prefixed_replacement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("dash_replace.txt");

        fs::write(&f, "placeholder\n").unwrap();

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": "placeholder",
            "replacement": "--header-value",
            "fixed_strings": true,
        });

        call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        let bytes = fs::read(&f).expect("read back replaced file");
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "--header-value\n");

        drop(dir);
    }

    /// NL-IT-10: `tpu_append_file` on a CRLF file with CRLF content produces
    /// clean CRLF, not \r\r\n.
    #[test]
    fn nl_it_10_append_crlf_file_no_double_cr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("append_crlf_target.txt");

        // Create a CRLF file.
        fs::write(&f, "line1\r\n").unwrap();

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "content": "line2\r\nline3\r\n",
        });

        call("tpu_append_file", &args).expect("tpu_append_file must succeed");

        let bytes = fs::read(&f).expect("read back appended file");
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains("\r\r"),
            "CRLF file must not contain double-CR after append; got: {text:?}"
        );
        assert_eq!(text, "line1\r\nline2\r\nline3\r\n");

        drop(dir);
    }

    /// RE-IT-1: `tpu_replace_in_file` expands `\n` in the replacement string
    /// to a real newline rather than writing the two-character sequence `\n`.
    #[test]
    fn re_it_1_replace_backslash_n_becomes_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("escape_replace.txt");

        fs::write(&f, "hello world\n").unwrap();

        // The replacement string contains the literal two characters \ and n,
        // as Copilot would send them.  The tool must expand this to a newline.
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": "hello world",
            "replacement": "hello\\nworld",
            "fixed_strings": true,
        });

        call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        let bytes = fs::read(&f).expect("read back replaced file");
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(
            text, "hello\nworld\n",
            "\\n in replacement must become a real newline; got: {text:?}"
        );
        assert!(
            !text.contains("\\n"),
            "literal backslash-n must not appear in output; got: {text:?}"
        );

        drop(dir);
    }

    /// RE-IT-2: `\t` in replacement expands to a real tab character.
    #[test]
    fn re_it_2_replace_backslash_t_becomes_tab() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("tab_replace.txt");

        fs::write(&f, "key: value\n").unwrap();

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": "key: value",
            "replacement": "key:\\tvalue",
            "fixed_strings": true,
        });

        call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        let text = String::from_utf8(fs::read(&f).unwrap()).unwrap();
        assert_eq!(text, "key:\tvalue\n");

        drop(dir);
    }

    /// RE-IT-3: `\\` (double backslash) in replacement produces a single
    /// literal backslash in the output.
    #[test]
    fn re_it_3_replace_double_backslash_becomes_single() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("bs_replace.txt");

        fs::write(&f, "path: here\n").unwrap();

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": "path: here",
            "replacement": "path:\\\\value",
            "fixed_strings": true,
        });

        call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        let text = String::from_utf8(fs::read(&f).unwrap()).unwrap();
        assert_eq!(text, "path:\\value\n");

        drop(dir);
    }

    /// RE-IT-4: capture-group references survive unescape and are expanded
    /// correctly by the regex engine.
    #[test]
    fn re_it_4_replace_capture_group_with_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("capture_replace.txt");

        fs::write(&f, "fn foo() {}\n").unwrap();

        // Wrap the matched function name with a blank line before it.
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": "(fn foo)",
            "replacement": "\\n$1",
        });

        call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        let text = String::from_utf8(fs::read(&f).unwrap()).unwrap();
        assert_eq!(text, "\nfn foo() {}\n");

        drop(dir);
    }

    /// WV-IT-1: success response from `tpu_write_file` includes mtime and size.
    #[test]
    fn wv_it_1_write_file_response_contains_stamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("stamp_write.txt");

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "content": "hello\nworld\n",
        });

        let result = call("tpu_write_file", &args).expect("tpu_write_file must succeed");
        assert!(
            result.contains("mtime="),
            "response must contain mtime=; got: {result:?}"
        );
        assert!(
            result.contains("size="),
            "response must contain size=; got: {result:?}"
        );
        assert!(
            result.contains("size=12"),
            "size must be 12; got: {result:?}"
        );

        drop(dir);
    }

    /// WV-IT-2: `tpu_replace_in_file` response includes mtime and size.
    #[test]
    fn wv_it_2_replace_response_contains_stamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("stamp_replace.txt");
        fs::write(&f, "foo\n").unwrap();

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": "foo",
            "replacement": "bar",
            "fixed_strings": true,
        });

        let result = call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");
        assert!(
            result.contains("mtime="),
            "response must contain mtime=; got: {result:?}"
        );
        assert!(result.contains("size=4"), "size must be 4; got: {result:?}");

        drop(dir);
    }

    /// WV-IT-3: `tpu_append_file` response includes mtime and size.
    #[test]
    fn wv_it_3_append_response_contains_stamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("stamp_append.txt");
        fs::write(&f, "line1\n").unwrap();

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "content": "line2\n",
        });

        let result = call("tpu_append_file", &args).expect("tpu_append_file must succeed");
        assert!(
            result.contains("mtime="),
            "response must contain mtime=; got: {result:?}"
        );
        assert!(
            result.contains("size=12"),
            "size must be 12; got: {result:?}"
        );

        drop(dir);
    }

    /// WV-IT-4: `tpu_edit_file` response includes mtime and size.
    #[test]
    fn wv_it_4_edit_response_contains_stamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("stamp_edit.txt");
        fs::write(&f, "aaa\nbbb\n").unwrap();

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "ops": [{ "op": "delete", "range": "2" }],
        });

        let result = call("tpu_edit_file", &args).expect("tpu_edit_file must succeed");
        assert!(
            result.contains("mtime="),
            "response must contain mtime=; got: {result:?}"
        );
        assert!(
            result.contains("size=4"),
            "size must be 4 after delete; got: {result:?}"
        );

        drop(dir);
    }

    /// WV-IT-5: `tpu_stat_file` returns valid JSON with size and mtime fields.
    #[test]
    fn wv_it_5_stat_file_returns_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("stat_target.txt");
        fs::write(&f, "0123456789").unwrap(); // 10 bytes

        let args = serde_json::json!({ "file": f.to_str().unwrap() });
        let result = call("tpu_stat_file", &args).expect("tpu_stat_file must succeed");

        let v: serde_json::Value =
            serde_json::from_str(&result).expect("tpu_stat_file must return valid JSON");
        assert_eq!(v["size"], 10, "size must be 10; full: {result:?}");
        assert!(
            v["mtime_epoch_ms"].as_u64().unwrap_or(0) > 0,
            "mtime_epoch_ms must be positive; full: {result:?}"
        );
        assert!(
            v["created_epoch_ms"].as_u64().unwrap_or(0) > 0,
            "created_epoch_ms must be positive; full: {result:?}"
        );
        assert_eq!(
            v["readonly"], false,
            "new file must not be readonly; full: {result:?}"
        );

        drop(dir);
    }

    /// WV-IT-6: mtime from `tpu_write_file` response stays within 2 s of a
    /// subsequent `tpu_stat_file` read (stamp survives the write).
    #[test]
    fn wv_it_6_write_stamp_matches_stat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("stamp_vs_stat.txt");
        let path_str = f.to_str().unwrap().to_owned();

        let write_args = serde_json::json!({ "file": &path_str, "content": "data\n" });
        let write_result =
            call("tpu_write_file", &write_args).expect("tpu_write_file must succeed");

        // Extract the mtime value from "mtime=NNN" in the response string.
        let mtime_write: u64 = write_result
            .split("mtime=")
            .nth(1)
            .and_then(|s| s.split([',', ']', ' ']).next())
            .and_then(|s| s.parse().ok())
            .expect("write result must contain parseable mtime=N");

        let stat_args = serde_json::json!({ "file": &path_str });
        let stat_result = call("tpu_stat_file", &stat_args).expect("tpu_stat_file must succeed");
        let stat: serde_json::Value = serde_json::from_str(&stat_result).unwrap();
        let mtime_stat = stat["mtime_epoch_ms"].as_u64().unwrap();

        // With verify_delay_ms=0 the stamp is not set explicitly; both values
        // come from OS metadata so they should agree within 2 seconds.
        assert!(
            mtime_write.abs_diff(mtime_stat) < 2_000,
            "write mtime {mtime_write} and stat mtime {mtime_stat} must be within 2s"
        );

        drop(dir);
    }
}
