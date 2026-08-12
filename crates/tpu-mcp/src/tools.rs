// Copyright (c) 2026, Michael Grier

//! Tool definitions and dispatch for the `tpu-mcp` MCP server.
//!
//! Each tool calls into the `tpu` library directly via the `call_*` dispatch
//! functions in this module.  File-reading tools (`read_file`, `read_head`,
//! `read_tail`, `read_file_binary`, `read_file_escaped`) return the raw file
//! content as plain text.  File-mutating tools (`write_file`, `replace_in_file`,
//! `edit_file`, `append_file`) return plain status text or diffs; `find` also
//! returns plain matching text. Structured JSON result objects are returned
//! only by tools that explicitly advertise them (`count_file`, `stat_file`,
//! `render_file`, `copy_file`) and by `setup` when `target` is provided.  On
//! failure all tools return an MCP error response.
//!
//! Tool set:
//! - `read_file`         — read a text file with encoding/line-ending normalisation
//! - `write_file`        — write text, preserving the file's existing encoding/line endings
//! - `create_file`       — create a new file, failing if the path already exists
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

// -- NDJSON tool result -------------------------------------------------------

/// The result of a tool call: structured NDJSON text and an error flag.
///
/// Tool responses use a mixed format.  The first line is always a JSON
/// `x-tpu-mcp-invocation` record describing the effective call (tool name
/// + sanitised arguments).  What follows depends on the tool type:
///
/// - **Mutating / structured tools** — a JSON body (zero or more
///   `{"reason":...}` data lines) then a `{"status":"success",...}` or
///   `{"status":"error","message":"..."}` trailer.
/// - **Read tools** — raw file content (not JSON); no trailer on success.
///
/// Not all output is therefore valid NDJSON; callers must be aware of the
/// mixed-mode contract and not attempt to parse every line as JSON.
///
/// `is_error` mirrors `CallToolResult.isError` so MCP clients can detect
/// failures without parsing the NDJSON trailer.
pub struct ToolResult {
    /// The full NDJSON response text.
    pub text: String,
    /// `true` when the underlying operation failed.
    pub is_error: bool,
}

impl ToolResult {
    /// Successful result with the given NDJSON text.
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    /// Error result: invocation header + `{"status":"error","message":"..."}`.
    pub fn error(header: &str, msg: &str) -> Self {
        let err_line = serde_json::json!({ "status": "error", "message": msg });
        let err_str = serde_json::to_string(&err_line).unwrap_or_default();
        Self {
            text: format!("{header}\n{err_str}"),
            is_error: true,
        }
    }
}

/// Build the first NDJSON line for any tool response.
///
/// Large text fields (`content`, `replacement`, `template`) are replaced by
/// a `"<N bytes>"` placeholder so the header stays compact.  All other
/// arguments pass through as-is.
///
/// Always includes a `"tpu_version"` field pinned to the `tpu-mcp` binary's
/// own `CARGO_PKG_VERSION` at compile time.  A caller can compare it against
/// the `tpu-mcp:setup:version=` HTML-comment marker embedded in
/// [`tpu::cmd::setup::guidance_body`]-injected copilot instructions to
/// detect when the running binary is out of sync with the guidance the
/// caller is following (M8 in `crates/tpu/CHECKLIST.md`).
fn invocation_header(tool: &str, args: &Value) -> String {
    const LARGE_FIELDS: &[&str] = &["content", "replacement", "template"];
    let mut sanitized = args.clone();
    for field in LARGE_FIELDS {
        if let Some(obj) = sanitized.as_object_mut() {
            if let Some(v) = obj.get(*field).and_then(|v| v.as_str()) {
                let n = v.len();
                obj.insert(
                    (*field).to_string(),
                    serde_json::Value::String(format!("<{n} bytes>")),
                );
            }
        }
    }
    serde_json::to_string(&serde_json::json!({
        "reason": "x-tpu-mcp-invocation",
        "tool":   tool,
        "args":   sanitized,
        "tpu_version": env!("CARGO_PKG_VERSION"),
    }))
    .unwrap_or_else(|_| format!("{{\"reason\":\"x-tpu-mcp-invocation\",\"tool\":{tool:?}}}"))
}

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
    "tpu_create_file",
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
    "tpu_doctor",
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
                 CLI shell) to diagnose and optionally repair the file. \
                 Line-ending awareness: pass `git_root` to additionally detect when the \
                 file's on-disk line endings differ from what git would materialise for \
                 that path (per .gitattributes / core.autocrlf / core.eol); when they do, \
                 the response is prefixed with a single `note:` line and the unchanged \
                 content follows. Run `tpu_doctor` with `fix: \"eol\"` to normalise.",
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
                    },
                    "git_root": {
                        "type": "string",
                        "description":
                            "Optional absolute path to a git repository root (no upward \
                             discovery). When set, the response is prefixed with a `note:` \
                             line if the file's on-disk line endings differ from git's \
                             expected convention for that path (per .gitattributes / \
                             core.autocrlf / core.eol). Opt-in; omit to skip all git checks."
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
                 were typing directly into the file.\n\n\
                 ESCAPE-HAZARD WARNING: a single stray backslash in the JSON you send \
                 (e.g. writing \\n when \\\\n was meant) is decoded to a real control \
                 character before this tool ever runs — the tool cannot tell that from \
                 an intentional newline, so this class of mistake is silent. When \
                 'content' contains backslash escapes, embedded quotes, or anything you \
                 are not fully confident is escaped correctly for JSON, set \
                 content_format:\"base64\" and send the exact bytes base64-encoded — \
                 this removes the escaping decision entirely. See 'content_format' below.",
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
                             file's existing convention unless line_ending is specified. \
                             When content_format is set, this is the encoded payload \
                             (e.g. a base64 string) instead of literal text — see \
                             content_format."
                    },
                    "content_format": {
                        "type": "string",
                        "enum": ["hex", "base64", "encoded"],
                        "description":
                            "If set, 'content' is decoded from this format instead of \
                             being used as literal JSON-string text, bypassing JSON's \
                             escape-sequence ambiguity entirely (see the ESCAPE-HAZARD \
                             warning above). Recommended: \"base64\" — its alphabet \
                             contains no backslashes, so there is no escaping decision \
                             to get wrong; the decoded bytes are used exactly as sent \
                             (after validating they are UTF-8 and normalising CRLF/CR to \
                             LF, same as the plain-text path). \"hex\" behaves the same. \
                             \"encoded\" applies tpu's own backslash-escape codec \
                             (\\n, \\t, \\r, \\\\, \\xHH, \\uXXXX) and therefore does NOT \
                             remove the JSON-escaping hazard — prefer base64 or hex. \
                             Omit for plain literal text (default, unchanged behaviour).",
                    },
                    "line_ending": {
                        "type": "string",
                        "enum": ["lf", "crlf", "cr"],
                        "description":
                            "Override the output line ending. Omit to preserve the file's \
                             existing convention. Cannot be used with binary content."
                    },
                    "git_root": {
                        "type": "string",
                        "description":
                            "Optional absolute path to a git repository root. When the \
                             server has line-ending normalisation enabled \
                             (tpu-mcp.normalizeLineEndings setting / --eol-normalize / \
                             TPU_EOL_NORMALIZE) and no explicit line_ending is given, the \
                             write denormalises to git's expected convention for this path \
                             (per .gitattributes / core.autocrlf / core.eol). Off by \
                             default — without the server setting this argument has no \
                             effect on writes."
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
            "name": "tpu_create_file",
            "description":
                "Create a NEW file and write UTF-8/LF text to it. Use this — not \
                 tpu_write_file — whenever the intent is to make a brand-new file: the \
                 name matches the intent and the call FAILS if the path already exists, \
                 so an existing file is never silently overwritten. To overwrite or \
                 modify a file that already exists, use tpu_write_file instead.\n\n\
                 New files are UTF-8 with LF line endings by default. Set line_ending to \
                 force CRLF/CR, or pass git_root to follow the repository's configured \
                 convention (per .gitattributes / core.autocrlf / core.eol) when the \
                 server has line-ending normalisation enabled. Parent directories are \
                 created as needed.\n\n\
                 ESCAPING: 'content' is the LITERAL text to write. The JSON-RPC transport \
                 already handles JSON string escaping; do not add a second layer. To \
                 insert a newline put a real newline in the JSON string.\n\n\
                 ESCAPE-HAZARD WARNING: a stray single backslash in the JSON you send \
                 (e.g. \\n where \\\\n was meant) is decoded to a real control character \
                 before this tool ever runs, and cannot be distinguished from an \
                 intentional newline. When 'content' contains backslash escapes, embedded \
                 quotes, or anything you are not fully confident is JSON-escaped \
                 correctly, set content_format:\"base64\" and send the exact bytes \
                 base64-encoded instead — see content_format.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description":
                            "Absolute path of the new file to create. Must not already \
                             exist; the call fails if it does."
                    },
                    "content": {
                        "type": "string",
                        "description":
                            "Full UTF-8 text content for the new file. Any CRLF or bare CR \
                             line endings are normalized to LF before processing, then \
                             written as LF unless line_ending (or git_root normalisation) \
                             specifies otherwise. When content_format is set, this is the \
                             encoded payload instead of literal text — see content_format."
                    },
                    "content_format": {
                        "type": "string",
                        "enum": ["hex", "base64", "encoded"],
                        "description":
                            "If set, 'content' is decoded from this format instead of \
                             being used as literal JSON-string text (see the \
                             ESCAPE-HAZARD warning above). Recommended: \"base64\" — no \
                             backslashes in its alphabet, so no escaping decision to get \
                             wrong. \"hex\" behaves the same. \"encoded\" applies tpu's own \
                             backslash-escape codec and does NOT remove the JSON-escaping \
                             hazard. Omit for plain literal text (default)."
                    },
                    "line_ending": {
                        "type": "string",
                        "enum": ["lf", "crlf", "cr"],
                        "description":
                            "Line ending for the new file. Omit for LF (the default for \
                             new files) or to defer to git_root normalisation."
                    },
                    "git_root": {
                        "type": "string",
                        "description":
                            "Optional absolute path to a git repository root. When the \
                             server has line-ending normalisation enabled \
                             (tpu-mcp.normalizeLineEndings setting / --eol-normalize / \
                             TPU_EOL_NORMALIZE) and no explicit line_ending is given, the \
                             new file is written with git's expected convention for this \
                             path (per .gitattributes / core.autocrlf / core.eol). Off by \
                             default — without the server setting this argument has no \
                             effect."
                    },
                    "allow_mojibake": {
                        "type": "boolean",
                        "description":
                            "If true, disable the write-time mojibake guard for this call. \
                             By default, content that contains mojibake patterns (any of \
                             the canonical Latin-1, punctuation, box-drawing, NBSP, or \
                             double-encoded fingerprints) is rejected with an error. Use \
                             this only when intentionally writing curated mojibake \
                             fixtures. Default: false."
                    }
                },
                "required": ["file", "content"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "tpu_replace_in_file",
            "description":
                "Perform an in-place substitution on a file. By default `pattern` is a \
                 fixed literal string — regex is opt-in, never implicit. Pass regex:true \
                 to interpret `pattern` as a Rust regex::bytes pattern applied to a \
                 LF-normalised view (CRLF is transparent — \\n in the pattern always means \
                 line feed). Capture groups (regex mode only, and only when the pattern \
                 has an explicit group): $0 (whole match), $1/$2/…, $name, $$ for a \
                 literal dollar sign — see the 'replacement' ESCAPING note below for when \
                 this applies. \
                 The original file is backed up to <file>.bak before writing. \
                 A zero-match run is a no-op: the file is not rewritten (mtime is \
                 preserved, no .bak is written) and the response includes count:0 \
                 plus a `warning` field so a caller can distinguish it from a real \
                 edit inline. The success response always includes `count` (the \
                 number of substitutions performed) so no follow-up count:true call \
                 is needed. \
                 Use count:true to count matches without modifying the file. \
                 Use dry_run:true to preview changes as a unified diff without writing. \
                 After a real write, a compact changed-region preview (new lines, with \
                 unified-diff-style hunk headers — cheap regardless of file size) is \
                 included in the response BY DEFAULT — no diff:true needed — as long as \
                 the change is small (see echo_max_lines below); this lets you catch a \
                 mistake (e.g. an escape-hazard corruption, see the ESCAPE-HAZARD warning) \
                 in the same turn instead of needing a follow-up tpu_read_file call. Pass \
                 diff:true for a full old/new unified diff instead.\n\n\
                 ESCAPING — RECOMMENDED DEFAULT: leave regex unset (or false) and send the \
                 search target as unescaped literal text (code, JSON, structured data, \
                 anything containing . ( ) [ ] { } * + ? | ^ $ \\). This is almost always \
                 what you want and avoids regex escaping entirely.\n\n\
                 ESCAPING — 'pattern' (regex:true only): escape ONLY regex metacharacters. \
                 Do NOT add an extra layer for JSON; the transport already handles that.\n\n\
                 ESCAPING — 'replacement': capture refs ($1, $name, $$) are honoured ONLY \
                 when regex:true AND the pattern has at least one capturing group. \
                 Otherwise — the default literal mode, and any regex without ( … ) — the \
                 replacement is written literally, so a bare $ (e.g. $5.00, $HOME, \
                 ${TOKEN}) is preserved rather than consumed. Add a capturing group and \
                 use $1 if you need a back-reference, and disambiguate a numbered \
                 reference from following literal text with braces: ${1}token, NOT \
                 $1token — the latter is parsed as a reference to a group *named* \
                 '1token', silently dropping both the substitution and the literal \
                 suffix. \
                 `\\n` and `\\r` both expand to LF (see the 'replacement' description \
                 below for why); `\\t` expands to TAB and `\\\\` to a single backslash. \
                 All other `\\X` pass through unchanged. Either a real newline in the \
                 JSON string OR the two characters backslash+n will produce a newline in \
                 the output — both are accepted.\n\n\
                 ESCAPE-HAZARD WARNING: because of the above, a stray single backslash in \
                 the JSON you send (e.g. \\n where \\\\n was meant) becomes a real newline \
                 before this tool ever runs — there is no way for the tool to tell that \
                 apart from an intentional one. When 'pattern' or 'replacement' contains \
                 backslash escapes, embedded quotes, or anything you are not fully \
                 confident is JSON-escaped correctly, set pattern_format:\"base64\" and/or \
                 replacement_format:\"base64\" and send the exact bytes base64-encoded — \
                 this removes the escaping decision entirely. See 'pattern_format' and \
                 'replacement_format' below.",
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
                            "Search target. By default this is a fixed literal string — \
                             every character, including `{`, `}`, `(`, `)`, `[`, `.`, `*`, \
                             `+`, `?`, is matched literally. Set regex:true to interpret \
                             this as a Rust regex::bytes pattern applied to the \
                             LF-normalised content instead (use (?s) for dot-all). When \
                             pattern_format is set, this is the encoded payload instead of \
                             literal text — see pattern_format."
                    },
                    "pattern_format": {
                        "type": "string",
                        "enum": ["hex", "base64", "encoded"],
                        "description":
                            "If set, 'pattern' is decoded from this format instead of \
                             being used as literal JSON-string text (see the \
                             ESCAPE-HAZARD warning above). Recommended: \"base64\" — no \
                             backslashes in its alphabet, so no escaping decision to get \
                             wrong. \"hex\" behaves the same. \"encoded\" applies tpu's own \
                             backslash-escape codec and does NOT remove the JSON-escaping \
                             hazard. Omit for plain literal text (default)."
                    },
                    "replacement": {
                        "type": "string",
                        "description":
                            "Replacement template. Capture-group references are honoured \
                             ONLY when regex:true AND the pattern has at least one \
                             capturing group: then $0 is the whole match, $1/$name are \
                             numbered/named groups, and $$ is a literal dollar sign. \
                             Otherwise (the default literal mode, and any regex without \
                             ( … )) the replacement is taken literally, so $ is written \
                             as-is (prices like $5.00, variables like $HOME, or ${TOKEN} \
                             placeholders are preserved). When you do use a numbered \
                             capture reference followed by literal text, disambiguate with \
                             braces: ${1}token, NOT $1token — the latter is parsed as a \
                             reference to a group *named* '1token'. \
                             Standard C-style backslash escapes are expanded first: \\n \
                             and \\r both become LF (see below), \\t becomes TAB, \\\\ \
                             becomes a single backslash; all other \\X sequences pass \
                             through unchanged. Any resulting CRLF or bare CR — from an \
                             escape or from a real CR/CRLF already in the JSON string — is \
                             then normalized to LF before substitution, which is why \\r \
                             ends up as LF rather than an actual carriage return: tpu \
                             never writes a bare CR into the LF-normalised substitution \
                             space. When replacement_format is set, this is the encoded \
                             payload instead — none of the above backslash-escape decoding \
                             applies; see replacement_format."
                    },
                    "replacement_format": {
                        "type": "string",
                        "enum": ["hex", "base64", "encoded"],
                        "description":
                            "If set, 'replacement' is decoded from this format, skipping \
                             tpu's own backslash-escape convenience decoding (\\n, \\t, \\r, \
                             \\\\) — the whole point of this channel is that the caller \
                             already specified the exact bytes (see the ESCAPE-HAZARD \
                             warning above). The decoded text still goes through the same \
                             CRLF/bare-CR -> LF normalisation as every other text payload, \
                             so a literal CR byte does NOT survive this channel — it always \
                             becomes LF, same as the plain-text path. \
                             Recommended: \"base64\". \"hex\" behaves the same. \"encoded\" \
                             applies tpu's own backslash-escape codec and does NOT remove \
                             the JSON-escaping hazard. Omit for plain literal text \
                             (default)."
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
                    "git_root": {
                        "type": "string",
                        "description":
                            "Optional absolute path to a git repository root. When the \
                             server has line-ending normalisation enabled \
                             (tpu-mcp.normalizeLineEndings setting / --eol-normalize / \
                             TPU_EOL_NORMALIZE) and no explicit line_ending is given, the \
                             write denormalises to git's expected convention for this path. \
                             Off by default."
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
                    "regex": {
                        "type": "boolean",
                        "description":
                            "If true, interpret `pattern` as a Rust regex::bytes pattern \
                             instead of a fixed literal string. Default: false. Regex is \
                             opt-in so an accidental metacharacter, or an ambiguous \
                             capture-group reference in `replacement` (e.g. $1token being \
                             parsed as group name '1token'), never silently changes what \
                             gets matched or replaced."
                    },
                    "echo_max_lines": {
                        "type": "integer",
                        "description":
                            "Maximum total changed (old-line-span + new-line-span) size \
                             for which the default changed-region echo is included in the \
                             response. When the actual change is at most this many lines, \
                             a compact preview (unified-diff-style hunk headers, new lines \
                             only — cheap regardless of file size, never a full-file diff) \
                             is prepended to the response automatically. Any individual \
                             echoed line longer than 500 bytes is itself truncated with a \
                             marker, so a small number of very long lines (minified JSON, a \
                             base64 blob, ...) still can't produce an unbounded response. \
                             When the line count is larger than this limit, the preview is \
                             omitted entirely and the status trailer instead reports \
                             'changed_lines' (the actual count) and 'diff_omitted':true — \
                             pass diff:true explicitly to see a full old/new unified diff \
                             regardless of size. Has no effect when diff:true is set \
                             (always shown then) or when count:true/dry_run:true is set. \
                             Default: 5."
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
                 tpu_replace_in_file instead (its default literal-string matching needs \
                 no escaping), because line numbers can shift between reads.\n\n\
                 All operation positions reference the original file; multiple ops in one \
                 call are applied without interference. The original file is backed up \
                 to <file>.bak before writing.\n\n\
                 ESCAPING (text mode): each op's 'data' is LITERAL text. The JSON \
                 transport already handles escaping; do not add a second layer. Put a \
                 real newline in the JSON string for a newline in the file. CRLF/CR in \
                 'data' is normalised to LF before the edit, then re-encoded to match \
                 the file's line-ending convention.\n\n\
                 ESCAPE-HAZARD WARNING: a stray single backslash in the JSON you send \
                 (e.g. \\n where \\\\n was meant) is decoded to a real control character \
                 before this tool ever runs, indistinguishable from an intentional \
                 newline. 'data_format' works in both text and binary mode: when an op's \
                 'data' contains backslash escapes, embedded quotes, or anything you are \
                 not fully confident is JSON-escaped correctly, set that op's \
                 data_format:\"base64\" and send the exact bytes base64-encoded — this \
                 removes the escaping decision entirely (in text mode the decoded bytes \
                 still get the usual CRLF/CR → LF normalisation).\n\n\
                 Each entry in 'ops' must have:\n\
                   op          — 'delete', 'insert', or 'splice'\n\
                   range       — 'N' or 'N-M' (required for delete/splice)\n\
                   offset      — 'N' (required for insert; position to insert before)\n\
                   data        — text or encoded bytes (required for insert/splice)\n\
                   data_format — 'hex', 'base64', or 'encoded' (optional; works in both \
                                 text and binary mode)\n\n\
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
                             data_format (optional; works in both text and binary mode).",
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
                                    "description": "Encoding of data. Works in both text \
                                     and binary mode; recommended (\"base64\") whenever \
                                     'data' contains backslash escapes or embedded quotes \
                                     you're not confident are JSON-escaped correctly."
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
                    "git_root": {
                        "type": "string",
                        "description":
                            "Optional absolute path to a git repository root. When the \
                             server has line-ending normalisation enabled \
                             (tpu-mcp.normalizeLineEndings setting / --eol-normalize / \
                             TPU_EOL_NORMALIZE) and no explicit line_ending is given, the \
                             write denormalises to git's expected convention for this path. \
                             Off by default."
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
                    },
                    "git_root": {
                        "type": "string",
                        "description":
                            "Optional absolute path to a git repository root. When set (and \
                             not in byte mode), the response is prefixed with a `note:` line \
                             if the file's on-disk line endings differ from git's expected \
                             convention for that path (per .gitattributes / core.autocrlf / \
                             core.eol). Opt-in."
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
                    },
                    "git_root": {
                        "type": "string",
                        "description":
                            "Optional absolute path to a git repository root. When set (and \
                             not in byte mode), the response is prefixed with a `note:` line \
                             if the file's on-disk line endings differ from git's expected \
                             convention for that path (per .gitattributes / core.autocrlf / \
                             core.eol). Opt-in."
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
                 File metadata (encoding name, BOM presence, line-ending style) is always \
                 included in the result under 'encoding', 'bom', and 'line_ending'. \n\n\
                 Returns a JSON object with the requested counts. When patterns are \
                 supplied their results appear in a 'patterns' sub-object keyed by label \
                 (e.g. {\"patterns\": {\"my-label\": 3}}).",
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
                            "Accepted for compatibility; has no effect. File metadata \
                             (encoding name, BOM presence, line-ending style) is always \
                             present in the result under the 'encoding', 'bom', and \
                             'line_ending' keys."
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
                 a newline to 'content' yourself.\n\n\
                 ESCAPE-HAZARD WARNING: a stray single backslash in the JSON you send \
                 (e.g. \\n where \\\\n was meant) is decoded to a real control character \
                 before this tool ever runs, and cannot be distinguished from an \
                 intentional newline. When 'content' contains backslash escapes, embedded \
                 quotes, or anything you are not fully confident is JSON-escaped \
                 correctly, set content_format:\"base64\" and send the exact bytes \
                 base64-encoded instead — see content_format.",
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
                             unless line_ending is specified. When content_format is set, \
                             this is the encoded payload instead of literal text — see \
                             content_format."
                    },
                    "content_format": {
                        "type": "string",
                        "enum": ["hex", "base64", "encoded"],
                        "description":
                            "If set, 'content' is decoded from this format instead of \
                             being used as literal JSON-string text (see the \
                             ESCAPE-HAZARD warning above). Recommended: \"base64\" — no \
                             backslashes in its alphabet, so no escaping decision to get \
                             wrong. \"hex\" behaves the same. \"encoded\" applies tpu's own \
                             backslash-escape codec and does NOT remove the JSON-escaping \
                             hazard. Omit for plain literal text (default)."
                    },
                    "line_ending": {
                        "type": "string",
                        "enum": ["lf", "crlf", "cr"],
                        "description":
                            "Override the output line ending for the combined file. Omit to \
                             preserve the file's existing convention."
                    },
                    "git_root": {
                        "type": "string",
                        "description":
                            "Optional absolute path to a git repository root. When the \
                             server has line-ending normalisation enabled \
                             (tpu-mcp.normalizeLineEndings setting / --eol-normalize / \
                             TPU_EOL_NORMALIZE) and no explicit line_ending is given, the \
                             write denormalises to git's expected convention for this path. \
                             Off by default."
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
                 or more patterns. By default each pattern is a fixed literal string — \
                 regex is opt-in, never implicit; set regex:true to interpret patterns \
                 as Rust regexes instead. Files are decoded with \
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
                            "Primary pattern to search for. By default this is a fixed \
                             literal string — every character, including `{`, `}`, `(`, \
                             `)`, `[`, `.`, `*`, `+`, `?`, is matched literally. Set \
                             regex:true to interpret every pattern as a Rust regex \
                             instead. At least one of 'pattern' or 'patterns' must be \
                             supplied."
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
                            "Absolute path to a file, directory, or wax glob to \
                             search. A directory must be paired with `glob` to \
                             select which files under it to search. At least one \
                             of 'path', 'paths', 'file', or 'files' must be supplied. \
                             'file' is accepted as an alias for this field."
                    },
                    "paths": {
                        "type": "array",
                        "description":
                            "Additional file paths, directories, or wax globs. \
                             Combined with 'path' (if present). When `glob` is \
                             supplied it applies to every directory entry in \
                             `path`/`paths`; literal file entries are searched as-is. \
                             'files' is accepted as an alias for this field.",
                        "items": { "type": "string" }
                    },
                    "file": {
                        "type": "string",
                        "description": "Alias for 'path'. Absolute path to a single file to search."
                    },
                    "files": {
                        "type": "array",
                        "description": "Alias for 'paths'. Additional file paths to search.",
                        "items": { "type": "string" }
                    },
                    "glob": {
                        "type": "string",
                        "description":
                            "Filename glob applied when a `path` is a directory. The \
                             directory is walked recursively and every file whose path \
                             relative to it matches the glob is searched. Use this for \
                             the common `find DIR -name PAT` shape, e.g. \
                             path:\"q:/src/foo/.scratch\", glob:\"**/*.ndjson\". \
                             Literal file paths supplied via `path`/`paths` are included \
                             as-is and are not filtered. Mutually exclusive with paths \
                             that themselves contain glob metacharacters (`*`, `?`, `[`, \
                             `{`)."
                    },
                    "all_match": {
                        "type": "boolean",
                        "description":
                            "When true, a line must match ALL supplied patterns to be \
                             emitted (AND mode).  Default false (OR mode)."
                    },
                    "regex": {
                        "type": "boolean",
                        "description":
                            "If true, interpret every pattern as a Rust regex instead of \
                             a fixed literal string. Default: false. Equivalent to the \
                             inverse of -F in grep (grep defaults to basic regex; here \
                             regex is opt-in)."
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
                 produce a warning record (returned in the final JSON result) and \
                 the operation continues with the next entry. Set on_error:'fail' to restore the legacy 'abort on \
                 first error' behaviour.\n\n\
                 Modes:\n\
                   single file       — `source` is a file path, `dest` is a file path \
                                       or an existing directory.\n\
                   directory tree    — `source` is a directory path; pass recursive:true. \
                                       `dest` is created if needed and the tree is \
                                       mirrored beneath it.\n\
                   glob expansion    — `source` contains `*`, `?`, `[`, `{`. Relative \
                                       patterns are resolved from the current working \
                                       directory; absolute patterns are anchored at their \
                                       non-glob prefix. Matches are copied flat into \
                                       `dest` (which must be a directory).\n\n\
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
                 tpu_write_file, tpu_replace_in_file, tpu_edit_file, tpu_append_file, \
                 tpu_render_file, or a targeted tpu_setup response (when called with \
                 `target`). A stale or mismatched mtime after a write likely indicates \
                 Windows Defender interference and means the operation should be retried.",
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
        },
        {
            "name": "tpu_doctor",
            "description":
                "Diagnose (and optionally repair) mojibake and encoding damage in one or \
                 more files. Use this when a file looks garbled (`Ã©` instead of `é`, \
                 `â€\"` instead of `—`, `â\"€` instead of `─`, stray `Â ` before \
                 numbers, …) or when a `tpu_read_file` call surfaced a `note: ... \
                 file appears to contain mojibake` warning. \n\n\
                 SCANS ONLY by default — returns a structured JSON report listing \
                 every flagged file with its detected encoding, per-pattern match counts, \
                 line/column locations, and whether a one-layer 'peel' repair would \
                 strictly improve the file. Safe to call on directories and globs; \
                 binary file extensions and `.git/` subtrees are skipped automatically. \n\n\
                 REPAIRS only when called with `fix: \"peel\"`. The repair is conservative: \
                 the file is rewritten only if the peel produces strictly fewer mojibake \
                 matches than the original. The original content is preserved at \
                 `<file>.bak` (the standard atomic-write backup). To preview without \
                 writing, leave `fix` unset and inspect `peel_suggested` in the report. \n\n\
                 Files containing the literal sentinel `encoding-check: allow-mojibake` \
                 are treated as legitimate (test fixtures, regex sources, docs about \
                 mojibake) and reported as clean. \n\n\
                 LINE ENDINGS: pass `git_root` to additionally detect files whose on-disk \
                 line endings differ from git's expected convention for their path (per \
                 .gitattributes / core.autocrlf / core.eol); such files are flagged with \
                 an `eol_mismatch` object in the report. Call with `fix: \"eol\"` (line \
                 endings only) or `fix: \"all\"` (peel + line endings) together with \
                 `git_root` to normalise them atomically with a `.bak` backup. \n\n\
                 When a teammate or another tool (e.g. PowerShell `Get-Content` / \
                 `Set-Content`, a misconfigured generator) appears to have introduced \
                 corruption, `git log -p -- <file>` will identify the introducing commit \
                 and therefore the offending writer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description":
                            "A file, directory, or shell-style glob to scan. Either `path` \
                             or `paths` must be provided; both may be combined."
                    },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description":
                            "Additional paths to scan, accumulated with `path`. Each may \
                             be a file, directory, or shell-style glob."
                    },
                    "fix": {
                        "type": "string",
                        "enum": ["none", "peel", "eol", "all"],
                        "description":
                            "Repair mode. `none` (default) reports only; `peel` applies a \
                             single-layer mojibake undo to any file whose peel produces \
                             strictly fewer matches than the original, rewriting it \
                             atomically with a `.bak` backup. `eol` normalises only line \
                             endings to git's expected convention; `all` does both peel \
                             and eol. `eol` and `all` require the `git_root` argument."
                    },
                    "git_root": {
                        "type": "string",
                        "description":
                            "Optional absolute path to a git repository root (no upward \
                             discovery). When set, doctor additionally reports files whose \
                             on-disk line endings differ from git's expected convention \
                             (per .gitattributes / core.autocrlf / core.eol) via an \
                             `eol_mismatch` field. Required when `fix` is `eol` or `all`."
                    },
                    "on_error": {
                        "type": "string",
                        "enum": ["fail", "warn"],
                        "description":
                            "How to handle per-entry walk errors (e.g. an inaccessible \
                             subdirectory). `warn` (default) collects warnings into the \
                             report and continues; `fail` stops on the first error."
                    }
                },
                "required": [],
                "anyOf": [
                    { "required": ["path"] },
                    { "required": ["paths"] }
                ]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true }
        }
    ])
}

// -- dispatch ------------------------------------------------------------------

/// Call a named tool with the given JSON arguments.
///
/// Always returns `Ok(ToolResult)`. Tool-level failures — including unknown
/// tool names — are represented as `ToolResult { is_error: true, .. }` rather
/// than `Err`. This ensures that the I/O worker sends `{"ok":...,"is_error":true}`
/// over the wire instead of `{"err":...}`, so the worker manager surfaces the
/// error without treating it as a protocol failure or triggering a respawn.
///
/// The `Err` path is kept in the signature for forward-compatibility but is
/// unreachable in current code.
pub fn call(
    name: &str,
    args: &Value,
    config: &ServerConfig,
) -> Result<ToolResult, Box<dyn std::error::Error>> {
    match name {
        "tpu_read_file" => Ok(call_read_file(args)),
        "tpu_write_file" => Ok(call_write_file(args, config)),
        "tpu_create_file" => Ok(call_create_file(args, config)),
        "tpu_replace_in_file" => Ok(call_replace_in_file(args, config)),
        "tpu_edit_file" => Ok(call_edit_file(args, config)),
        "tpu_read_file_binary" => Ok(call_read_file_binary(args)),
        "tpu_read_file_escaped" => Ok(call_read_file_escaped(args)),
        "tpu_validate_file" => Ok(call_validate_file(args)),
        "tpu_read_head" => Ok(call_read_head(args)),
        "tpu_read_tail" => Ok(call_read_tail(args)),
        "tpu_count_file" => Ok(call_count_file(args)),
        "tpu_append_file" => Ok(call_append_file(args, config)),
        "tpu_copy_file" => Ok(call_copy_file(args, config)),
        "tpu_render_file" => Ok(call_render_file(args, config)),
        "tpu_setup" => Ok(call_setup(args, config)),
        "tpu_find" => Ok(call_find(args, config)),
        "tpu_stat_file" => Ok(call_stat_file(args)),
        "tpu_doctor" => Ok(call_doctor(args, config)),
        _ => Ok(ToolResult::error(
            &invocation_header(name, args),
            &format!("unknown tool: {name}"),
        )),
    }
}

// -- individual tool implementations ------------------------------------------

/// Extract the optional per-call `git_root` argument as a path.
///
/// Like every other path-typed argument, `git_root` is run through
/// [`normalize_file_path`] so clients may pass it as a `file://` URI (with
/// percent-encoding) — common in VS Code integrations — and have it resolve to
/// the same on-disk path that plain filesystem paths do.
fn git_root_arg(args: &Value) -> Option<std::path::PathBuf> {
    args.get("git_root")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| std::path::PathBuf::from(normalize_file_path(s)))
}

thread_local! {
    /// Per-worker cache of opened [`tpu::git::GitEol`] handles, keyed by the
    /// canonicalised `git_root`.  An agent session issues many reads against
    /// the same repository; opening the repo (and loading its index) on every
    /// call is wasteful, so we open once and reuse.
    ///
    /// The worker loop is single-threaded, so a `thread_local` `RefCell` needs
    /// no locking and `GitEol` need not be `Sync`.  Entries live for the
    /// process lifetime; a `.gitattributes`/config change mid-session is not
    /// observed — acceptable for a best-effort advisory and for opt-in
    /// normalisation within a single session.
    static EOL_REPO_CACHE: std::cell::RefCell<
        std::collections::HashMap<std::path::PathBuf, Option<std::rc::Rc<tpu::git::GitEol>>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Open (or reuse a cached) [`tpu::git::GitEol`] for `root`.  Returns `None`
/// when the repository cannot be opened or has no working tree.
fn cached_git_eol(root: &std::path::Path) -> Option<std::rc::Rc<tpu::git::GitEol>> {
    let key = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    EOL_REPO_CACHE.with(|cache| {
        if let Some(hit) = cache.borrow().get(&key) {
            return hit.clone();
        }
        let opened = tpu::git::GitEol::open(root)
            .ok()
            .flatten()
            .map(std::rc::Rc::new);
        cache.borrow_mut().insert(key, opened.clone());
        opened
    })
}

/// When a `git_root` is supplied and the file's on-disk line endings differ
/// from git's expected convention, prepend a single `note:` line (identical to
/// the one `tpu read --git-root` emits) ahead of the returned content.  The
/// note line is part of the response *preamble*, like the invocation header,
/// and is omitted entirely when there is no mismatch.
fn prepend_eol_note(args: &Value, file: &str, content: String) -> String {
    let Some(root) = git_root_arg(args) else {
        return content;
    };
    let Some(git) = cached_git_eol(&root) else {
        return content;
    };
    match git.advisory_note(std::path::Path::new(file)) {
        Some(note) => format!("{note}\n{content}"),
        None => content,
    }
}

/// Resolve the write-time line-ending override for a mutating MCP tool.
///
/// Thin wrapper over the shared [`tpu::git::resolve_write_override`] (also used
/// by the `tpu` CLI): an explicit `line_ending` argument always wins;
/// otherwise, when the server has line-ending normalisation enabled
/// (`config.eol_normalize`) and the call supplies a `git_root`, the override is
/// git's expected convention for the file.  Returns `Ok(None)` when neither
/// applies.
fn eol_write_override(
    args: &Value,
    file: &str,
    config: &ServerConfig,
) -> Result<Option<tpu::encoding::LineEnding>, Box<dyn std::error::Error>> {
    let explicit = args.get("line_ending").and_then(|v| v.as_str());
    let git_root = git_root_arg(args);
    let git_root = if config.eol_normalize {
        git_root.as_deref()
    } else {
        None
    };
    tpu::git::resolve_write_override(explicit, std::path::Path::new(file), git_root, true)
}

fn call_read_file(args: &Value) -> ToolResult {
    let header = invocation_header("tpu_read_file", args);
    let inner = || -> Result<String, Box<dyn std::error::Error>> {
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
        let content = String::from_utf8(buf).map_err(|e| format!("read: non-UTF-8 output: {e}"))?;
        Ok(prepend_eol_note(args, &file, content))
    };
    match inner() {
        Ok(content) => ToolResult::ok(format!("{header}\n{content}")),
        Err(e) => ToolResult::error(&header, &e.to_string()),
    }
}

fn call_write_file(args: &Value, config: &ServerConfig) -> ToolResult {
    let header = invocation_header("tpu_write_file", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
        let file = resolve_file_arg(args)?;
        let content = decode_content_arg(args, "content")?;
        let path = std::path::Path::new(&file);

        let le_override = eol_write_override(args, &file, config)?;

        if let Some(validates) = args.get("validate").and_then(|v| v.as_array()) {
            let pairs = flatten_validate_pairs(validates)?;
            tpu::cmd::validate::run_all(&pairs, path, false, tpu::IoMode::Buffered)?;
        }

        let diff = args.get("diff").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut diff_buf: Vec<u8> = Vec::new();
        let diff_out: Option<&mut dyn std::io::Write> =
            if diff { Some(&mut diff_buf) } else { None };

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
        let status = serde_json::json!({
            "status": "success",
            "file": file,
            "mtime_epoch_ms": stamp.mtime_epoch_ms,
            "size": stamp.size,
        });
        let status_line = serde_json::to_string(&status)?;
        if diff && !diff_buf.is_empty() {
            let diff_text = String::from_utf8_lossy(&diff_buf);
            let sep = diff_separator(&diff_text);
            Ok(ToolResult::ok(format!(
                "{header}\n{diff_text}{sep}{status_line}"
            )))
        } else {
            Ok(ToolResult::ok(format!("{header}\n{status_line}")))
        }
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_create_file(args: &Value, config: &ServerConfig) -> ToolResult {
    let header = invocation_header("tpu_create_file", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
        let file = resolve_file_arg(args)?;
        let content = decode_content_arg(args, "content")?;
        let path = std::path::Path::new(&file);

        let le_override = eol_write_override(args, &file, config)?;
        let policy = mojibake_policy_from_args(args);

        tpu::cmd::create::run(
            path,
            &content,
            tpu::encoding::OutputEncoding::Preserve,
            tpu::encoding::BomPolicy::default(),
            le_override,
            tpu::IoMode::Buffered,
            policy,
        )?;
        let stamp = stamp_and_verify(path, config.verify_delay_ms)?;
        let status = serde_json::json!({
            "status": "success",
            "file": file,
            "mtime_epoch_ms": stamp.mtime_epoch_ms,
            "size": stamp.size,
        });
        let status_line = serde_json::to_string(&status)?;
        Ok(ToolResult::ok(format!("{header}\n{status_line}")))
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_replace_in_file(args: &Value, config: &ServerConfig) -> ToolResult {
    let header = invocation_header("tpu_replace_in_file", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
        reject_removed_fixed_strings_arg(args)?;
        let file = resolve_file_arg(args)?;
        let pattern = decode_pattern_arg(args, "pattern")?;
        let replacement = decode_replacement_arg(args)?;
        let path = std::path::Path::new(&file);

        let multiline = args
            .get("multiline")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let regex = args.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);
        let le_override = eol_write_override(args, &file, config)?;
        let diff = args.get("diff").and_then(|v| v.as_bool()).unwrap_or(false);
        let count = args.get("count").and_then(|v| v.as_bool()).unwrap_or(false);
        let dry_run = args
            .get("dry_run")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut diff_buf: Vec<u8> = Vec::new();
        // The full whole-file diff clones the entire normalised file (see
        // ChangedRegion's doc comment) and stays strictly opt-in: only
        // requested when explicitly asked for (diff:true) or when
        // previewing (dry_run:true, which has always shown a preview).
        // The default changed-region echo below is built from `regions`
        // instead, which is cheap regardless of file size.
        let diff_out: Option<&mut dyn std::io::Write> = if count {
            None
        } else if diff || dry_run {
            Some(&mut diff_buf)
        } else {
            None
        };
        let echo_max_lines = args
            .get("echo_max_lines")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize;
        let mut regions: Vec<tpu::cmd::replace::ChangedRegion> = Vec::new();
        let regions_req = if count {
            None
        } else {
            Some(tpu::cmd::replace::RegionsRequest {
                regions_out: &mut regions,
                // diff:true always renders from diff_buf (see below), not
                // `regions`, so no text needs to be retained in that case;
                // otherwise bound retained text to the same threshold that
                // gates whether it's ever shown, rather than the full size
                // of every match's replacement.
                text_budget_lines: Some(if diff { 0 } else { echo_max_lines }),
            })
        };

        let n = tpu::cmd::replace::run(
            path,
            &pattern,
            replacement.as_bytes(),
            diff_out,
            regions_req,
            tpu::cmd::replace::ReplaceOptions {
                multiline,
                regex,
                line_ending_override: le_override,
                count_only: count,
                dry_run,
                io_mode: tpu::IoMode::Buffered,
                policy: mojibake_policy_from_args(args),
            },
        )?;

        if count {
            let line = serde_json::to_string(&serde_json::json!({
                "status": "success", "count": n,
            }))?;
            return Ok(ToolResult::ok(format!("{header}\n{line}")));
        }
        if dry_run {
            let status_line = serde_json::to_string(&serde_json::json!({
                "status": if diff_buf.is_empty() { "success" } else { "success" },
                "changed": !diff_buf.is_empty(),
            }))?;
            if diff_buf.is_empty() {
                return Ok(ToolResult::ok(format!("{header}\n{status_line}")));
            }
            let diff_text = String::from_utf8_lossy(&diff_buf);
            let sep = diff_separator(&diff_text);
            return Ok(ToolResult::ok(format!(
                "{header}\n{diff_text}{sep}{status_line}"
            )));
        }
        // File was modified.
        delete_bak_if_exists(&file);
        let stamp = stamp_and_verify(Path::new(&file), config.verify_delay_ms)?;
        let changed_lines: usize = regions
            .iter()
            .map(|r| (r.end_line - r.start_line + 1) + r.new_line_count)
            .sum();
        // Explicit diff:true always shows the full diff regardless of size;
        // otherwise, echo automatically only when the change is small enough
        // to be a cheap, high-value safety net rather than a wall of text.
        let should_echo = diff || changed_lines <= echo_max_lines;
        let mut status = serde_json::json!({
            "status": "success",
            "file": file,
            "mtime_epoch_ms": stamp.mtime_epoch_ms,
            "size": stamp.size,
            "count": n,
            "changed_lines": changed_lines,
        });
        // A zero-match replace returns success (no error occurred) but did
        // nothing -- make that visible inline so a caller doesn't mistake
        // it for a real edit. See CHECKLIST.md M7 for background.
        if n == 0 {
            status["warning"] = serde_json::json!(
                "pattern matched 0 times; file not modified (matching is literal by default; \
                 pass regex:true for regex)"
            );
        }
        if !should_echo {
            status["diff_omitted"] = serde_json::json!(true);
        }
        let status_line = serde_json::to_string(&status)?;
        if !should_echo {
            return Ok(ToolResult::ok(format!("{header}\n{status_line}")));
        }
        // Prefer the full unified diff when diff:true was requested (it
        // populated diff_buf); otherwise render the cheap changed-region
        // echo built from `regions` -- no full-file clone involved.
        let echo_text = if diff && !diff_buf.is_empty() {
            String::from_utf8_lossy(&diff_buf).into_owned()
        } else {
            render_changed_regions(&regions)
        };
        if echo_text.is_empty() {
            return Ok(ToolResult::ok(format!("{header}\n{status_line}")));
        }
        let sep = diff_separator(&echo_text);
        Ok(ToolResult::ok(format!(
            "{header}\n{echo_text}{sep}{status_line}"
        )))
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_edit_file(args: &Value, config: &ServerConfig) -> ToolResult {
    let header = invocation_header("tpu_edit_file", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
        let file = resolve_file_arg(args)?;
        let path = std::path::Path::new(&file);
        let binary = args
            .get("binary")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let le_override = eol_write_override(args, &file, config)?;

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
        let status = serde_json::json!({
            "status": "success",
            "file": file,
            "mtime_epoch_ms": stamp.mtime_epoch_ms,
            "size": stamp.size,
        });
        let status_line = serde_json::to_string(&status)?;
        if diff && !diff_buf.is_empty() {
            let diff_text = String::from_utf8_lossy(&diff_buf);
            let sep = diff_separator(&diff_text);
            Ok(ToolResult::ok(format!(
                "{header}\n{diff_text}{sep}{status_line}"
            )))
        } else {
            Ok(ToolResult::ok(format!("{header}\n{status_line}")))
        }
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_read_file_binary(args: &Value) -> ToolResult {
    let header = invocation_header("tpu_read_file_binary", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
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
            let result_line = serde_json::to_string(&serde_json::json!({
                "reason": "x-tpu-mcp-result",
                "encoding": "bytes-base64",
                "content": content,
                "hashes": hashes_json,
            }))?;
            let status_line = serde_json::to_string(&serde_json::json!({"status":"success"}))?;
            Ok(ToolResult::ok(format!(
                "{header}\n{result_line}\n{status_line}"
            )))
        } else {
            // Return 7-bit-clean escaped string — mixed mode (header + content).
            let content = tpu::escape::encode_bytes(slice);
            Ok(ToolResult::ok(format!("{header}\n{content}")))
        }
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_read_file_escaped(args: &Value) -> ToolResult {
    let header = invocation_header("tpu_read_file_escaped", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
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
        let content =
            String::from_utf8(buf).map_err(|e| format!("readex: non-UTF-8 output: {e}"))?;
        Ok(ToolResult::ok(format!("{header}\n{content}")))
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_read_head(args: &Value) -> ToolResult {
    let header = invocation_header("tpu_read_head", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
        let file = resolve_file_arg(args)?;
        let path = std::path::Path::new(&file);

        let byte_mode = args.get("bytes").and_then(|v| v.as_u64());
        let mode = if let Some(n) = byte_mode {
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
        let content = String::from_utf8(buf).map_err(|e| format!("head: non-UTF-8 output: {e}"))?;
        // The EOL advisory only makes sense for line-oriented output; a byte
        // slice has no notion of "the file's line endings" and prefixing a
        // note would corrupt a byte-exact head.
        let content = if byte_mode.is_some() {
            content
        } else {
            prepend_eol_note(args, &file, content)
        };
        Ok(ToolResult::ok(format!("{header}\n{content}")))
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_read_tail(args: &Value) -> ToolResult {
    let header = invocation_header("tpu_read_tail", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
        let file = resolve_file_arg(args)?;
        let path = std::path::Path::new(&file);

        let byte_mode = args.get("bytes").and_then(|v| v.as_u64());
        let mode = if let Some(n) = byte_mode {
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
        let content = String::from_utf8(buf).map_err(|e| format!("tail: non-UTF-8 output: {e}"))?;
        // See `call_read_head`: the EOL advisory is line-oriented and must not
        // be prefixed onto a byte-exact tail.
        let content = if byte_mode.is_some() {
            content
        } else {
            prepend_eol_note(args, &file, content)
        };
        Ok(ToolResult::ok(format!("{header}\n{content}")))
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_count_file(args: &Value) -> ToolResult {
    let header = invocation_header("tpu_count_file", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
        let file = resolve_file_arg(args)?;
        let path = std::path::Path::new(&file);

        let lines = args.get("lines").and_then(|v| v.as_bool()).unwrap_or(false);
        let words = args.get("words").and_then(|v| v.as_bool()).unwrap_or(false);
        let chars = args.get("chars").and_then(|v| v.as_bool()).unwrap_or(false);
        let bytes = args.get("bytes").and_then(|v| v.as_bool()).unwrap_or(false);
        // Stats (encoding/bom/line_ending) are always emitted by the MCP tool
        // regardless of the caller-supplied flag, matching the advertised contract.
        let stats = true;

        let mut patterns: Vec<String> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        if let Some(entries) = args.get("patterns").and_then(|v| v.as_array()) {
            for entry in entries {
                let pattern = entry
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or("patterns entry missing 'pattern' field")?;
                // Always push a label for every pattern to keep vec positions
                // aligned with `patterns`; default to the pattern string itself
                // (matching count::run's own fallback behaviour).
                let label = entry
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or(pattern);
                patterns.push(pattern.to_owned());
                labels.push(label.to_owned());
            }
        }
        // Collect all user-supplied pattern labels into a set.  This is used
        // only to decide whether to initialise a "patterns" sub-object on
        // result_obj (see below); the actual per-metric routing is driven by
        // standard_metric_names and standard_metrics_placed, not by this set.
        let pattern_label_set: std::collections::HashSet<String> = labels.iter().cloned().collect();

        // Determine the expected standard metric names from the enabled flags.
        // count::run always emits standard metrics *before* pattern metrics, so
        // the first occurrence of each standard-metric name is authoritative for
        // the top-level result object.  Any subsequent occurrence of the same name
        // (i.e. a pattern label that collides with a standard metric) is routed
        // to the "patterns" sub-object, preserving the real metric value.
        // Mirror count::run's "emit all four when none are explicitly requested"
        // rule so the fold step knows which metric names to expect at the top level.
        let any_standard = lines || words || chars || bytes;
        let emit_lines = lines || !any_standard;
        let emit_words = words || !any_standard;
        let emit_chars = chars || !any_standard;
        let emit_bytes = bytes || !any_standard;
        let standard_metric_names: std::collections::HashSet<&str> = {
            let mut s = std::collections::HashSet::new();
            if emit_lines {
                s.insert("lines");
            }
            if emit_words {
                s.insert("words");
            }
            if emit_chars {
                s.insert("chars");
            }
            if emit_bytes {
                s.insert("bytes");
            }
            // stats is always true for the MCP tool; stats metrics are always expected.
            s.insert("encoding");
            s.insert("bom");
            s.insert("line_ending");
            s
        };

        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let writer = SharedBufWriter(buf.clone());
        let mut out = tpu::output::json_output_to(Box::new(writer));

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
        let raw = buf.lock().unwrap().clone();
        let ndjson = String::from_utf8(raw).map_err(|e| format!("count: non-UTF-8 output: {e}"))?;

        // Fold each {"reason":"data","metric":M,...} line into a single result
        // object.  Standard metrics use a "count" field; encoding/bom/line_ending
        // use a "value" field.
        //
        // Routing rules:
        //  - A metric in standard_metric_names whose name hasn't been placed yet →
        //    top-level key (authoritative standard value).
        //  - Any other metric (pattern label, or a second occurrence of a name that
        //    matches a standard metric) → "patterns" sub-object.
        let mut result_obj = serde_json::json!({"reason": "x-tpu-mcp-result"});
        if !pattern_label_set.is_empty() {
            result_obj["patterns"] = serde_json::json!({});
        }
        let mut standard_metrics_placed: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for line in ndjson.lines() {
            if line.is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(line)
                .map_err(|e| format!("count: malformed JSON line {line:?}: {e}"))?;
            let metric = match parsed.get("metric").and_then(|v| v.as_str()) {
                Some(m) => m.to_owned(),
                None => continue,
            };
            let val = if let Some(count) = parsed.get("count") {
                count.clone()
            } else if let Some(value) = parsed.get("value") {
                value.clone()
            } else {
                continue;
            };
            // Route the metric to the top-level only when it is a known standard
            // metric and hasn't been placed yet.  All other metrics (pattern
            // labels, or a label that duplicates a standard metric name) go to
            // the "patterns" sub-object.
            if standard_metric_names.contains(metric.as_str())
                && standard_metrics_placed.insert(metric.clone())
            {
                result_obj[metric] = val;
            } else if result_obj.get("patterns").is_some() {
                result_obj["patterns"][&metric] = val;
            }
        }

        let result_line = serde_json::to_string(&result_obj)?;
        let status_line = serde_json::to_string(&serde_json::json!({"status":"success"}))?;
        Ok(ToolResult::ok(format!(
            "{header}\n{result_line}\n{status_line}"
        )))
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_append_file(args: &Value, config: &ServerConfig) -> ToolResult {
    let header = invocation_header("tpu_append_file", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
        let file = resolve_file_arg(args)?;
        let content = decode_content_arg(args, "content")?;
        let path = std::path::Path::new(&file);

        let le_override = eol_write_override(args, &file, config)?;
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
                let diff_text = String::from_utf8_lossy(&diff_buf);
                let sep = diff_separator(&diff_text);
                let status = serde_json::json!({"status":"success","file":file,"changed":true});
                let status_line = serde_json::to_string(&status)?;
                return Ok(ToolResult::ok(format!(
                    "{header}\n{diff_text}{sep}{status_line}"
                )));
            }
            let status = serde_json::json!({"status":"success","file":file,"changed":false});
            let status_line = serde_json::to_string(&status)?;
            return Ok(ToolResult::ok(format!("{header}\n{status_line}")));
        }

        tpu::cmd::append::run(
            path,
            &content,
            le_override,
            None,
            tpu::IoMode::Buffered,
            mojibake_policy_from_args(args),
        )?;
        delete_bak_if_exists(&file);
        let stamp = stamp_and_verify(path, config.verify_delay_ms)?;
        let status = serde_json::json!({
            "status": "success",
            "file": file,
            "mtime_epoch_ms": stamp.mtime_epoch_ms,
            "size": stamp.size,
        });
        let status_line = serde_json::to_string(&status)?;
        Ok(ToolResult::ok(format!("{header}\n{status_line}")))
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_find(args: &Value, config: &ServerConfig) -> ToolResult {
    let header = invocation_header("tpu_find", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
        reject_removed_fixed_strings_arg(args)?;
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

        // Collect paths: primary "path"/"file" + optional "paths"/"files"
        // array; normalise URIs. "file"/"files" are accepted as aliases of
        // "path"/"paths" since every other tool in this server takes a
        // singular "file" argument and callers routinely reach for that name
        // here too.
        let mut all_paths: Vec<String> = Vec::new();
        for key in ["path", "file"] {
            if let Some(p) = args.get(key).and_then(|v| v.as_str()) {
                all_paths.push(normalize_file_path(p));
            }
        }
        for key in ["paths", "files"] {
            if let Some(arr) = args.get(key).and_then(|v| v.as_array()) {
                for p in arr {
                    if let Some(s) = p.as_str() {
                        all_paths.push(normalize_file_path(s));
                    }
                }
            }
        }

        if all_patterns.is_empty() {
            return Err("find: at least one pattern is required".into());
        }
        if all_paths.is_empty() {
            return Err(
                "find: at least one path is required ('path'/'paths', or the \
                 'file'/'files' aliases)"
                    .into(),
            );
        }

        let regex = args.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);
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

        let glob = args.get("glob").and_then(|v| v.as_str()).map(str::to_owned);

        let pattern_refs: Vec<&str> = all_patterns.iter().map(String::as_str).collect();
        let path_refs: Vec<&str> = all_paths.iter().map(String::as_str).collect();

        let on_error = match args.get("on_error") {
            None => config.default_on_error,
            Some(v) => match v.as_str() {
                Some("fail") => tpu::cmd::copy::OnError::Fail,
                Some("warn") => tpu::cmd::copy::OnError::Warn,
                Some(other) => {
                    return Err(format!(
                        "invalid value for `on_error`: {other:?}; expected \"warn\" or \"fail\""
                    )
                    .into());
                }
                None => {
                    return Err(format!(
                        "invalid value for `on_error`: expected a string, got {v}"
                    )
                    .into());
                }
            },
        };
        let mut walk_warnings: Vec<String> = Vec::new();

        let mut buf: Vec<u8> = Vec::new();
        let result = tpu::cmd::find::run_with_policy(
            &path_refs,
            &pattern_refs,
            glob.as_deref(),
            tpu::cmd::find::FindOptions {
                regex,
                multiline,
                ignore_case,
                all_match,
                invert,
                lines_before: before,
                lines_after: after,
                count_only: count,
                numbers,
                io_mode: tpu::IoMode::Buffered,
            },
            &mut buf,
            on_error,
            &mut walk_warnings,
        );

        match result {
            Ok(_) => {
                let mut content =
                    String::from_utf8(buf).map_err(|e| format!("find: non-UTF-8 output: {e}"))?;
                // Ensure content ends with newline so the status trailer is on its own line.
                if !content.is_empty() && !content.ends_with('\n') {
                    content.push('\n');
                }
                // Build status with optional warnings array.
                let warnings_json: Vec<serde_json::Value> = match config.progress_detail {
                    ProgressDetail::EachFile => walk_warnings
                        .iter()
                        .map(|w| serde_json::Value::String(w.clone()))
                        .collect(),
                    ProgressDetail::Summary => {
                        let n = walk_warnings.len();
                        if n > 0 {
                            vec![serde_json::Value::String(format!(
                                "{n} path(s) skipped (use progressDetail=each-file to list)"
                            ))]
                        } else {
                            vec![]
                        }
                    }
                };
                let mut status = serde_json::json!({ "status": "success" });
                if !warnings_json.is_empty() {
                    status["warnings"] = serde_json::Value::Array(warnings_json);
                }
                let status_line = serde_json::to_string(&status)?;
                Ok(ToolResult::ok(format!("{header}\n{content}{status_line}")))
            }
            Err(e) => {
                let msg = if walk_warnings.is_empty() {
                    e.to_string()
                } else {
                    format!("{}\n{e}", walk_warnings.join("\n"))
                };
                Err(msg.into())
            }
        }
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_copy_file(args: &Value, config: &ServerConfig) -> ToolResult {
    let header = invocation_header("tpu_copy_file", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
        let source = normalize_file_path(require_str(args, "source")?);
        let dest = normalize_file_path(require_str(args, "dest")?);
        let recursive = args
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let overwrite = args
            .get("overwrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let on_error = match args.get("on_error") {
            None => config.default_on_error,
            Some(v) => match v.as_str() {
                Some("fail") => tpu::cmd::copy::OnError::Fail,
                Some("warn") => tpu::cmd::copy::OnError::Warn,
                Some(other) => {
                    return Err(format!(
                        "invalid value for `on_error`: {other:?}; expected \"warn\" or \"fail\""
                    )
                    .into());
                }
                None => {
                    return Err(format!(
                        "invalid value for `on_error`: expected a string, got {v}"
                    )
                    .into());
                }
            },
        };
        let opts = tpu::cmd::copy::CopyOptions {
            recursive,
            overwrite,
            on_error,
        };

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
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
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
        let warn_lines: Vec<String> = raw
            .lines()
            .map(|l| l.strip_prefix("warning: ").unwrap_or(l).to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let mut result_obj = serde_json::json!({
            "reason":   "x-tpu-mcp-result",
            "copied":   report.copied,
            "skipped":  report.skipped,
            "warnings": report.warnings,
        });
        if matches!(config.progress_detail, ProgressDetail::EachFile) {
            result_obj["log"] = serde_json::Value::Array(
                warn_lines
                    .iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            );
        }
        let result_line = serde_json::to_string(&result_obj)?;
        let status_line = serde_json::to_string(&serde_json::json!({"status":"success"}))?;
        Ok(ToolResult::ok(format!(
            "{header}\n{result_line}\n{status_line}"
        )))
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_render_file(args: &Value, config: &ServerConfig) -> ToolResult {
    let header = invocation_header("tpu_render_file", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
        use std::collections::BTreeMap;
        let output = normalize_file_path(require_str(args, "output")?);
        // Normalize CRLF → LF at the MCP boundary, consistent with other write tools.
        let template_inline_owned = match args.get("template") {
            None => None,
            Some(v) => {
                let s = v
                    .as_str()
                    .ok_or_else(|| format!("render: `template` must be a string, got {v}"))?;
                Some(s.replace("\r\n", "\n").replace('\r', "\n"))
            }
        };
        let template_inline = template_inline_owned.as_deref();
        let template_file = match args.get("template_file") {
            None => None,
            Some(v) => {
                let s = v
                    .as_str()
                    .ok_or_else(|| format!("render: `template_file` must be a string, got {v}"))?;
                Some(normalize_file_path(s))
            }
        };
        let mut vars: BTreeMap<String, String> = BTreeMap::new();
        if let Some(vars_val) = args.get("vars") {
            let map = vars_val
                .as_object()
                .ok_or_else(|| format!("render: `vars` must be an object, got {vars_val}"))?;
            for (k, v) in map {
                // Enforce the same key constraints as the CLI parser.
                if k.is_empty()
                    || !k
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Err(format!(
                        "render: vars key {k:?} may only contain ASCII letters, digits, '_' or '-'"
                    )
                    .into());
                }
                let val = v
                    .as_str()
                    .ok_or_else(|| format!("render: vars[{k}]: value must be a string"))?;
                // Normalize CRLF → LF so variable values don't produce doubled
                // carriage returns when written to CRLF-convention destination files.
                vars.insert(k.clone(), val.replace("\r\n", "\n").replace('\r', "\n"));
            }
        }
        let missing = match args.get("missing") {
            None => tpu::cmd::render::MissingPolicy::Error,
            Some(v) => match v.as_str() {
                Some("error") => tpu::cmd::render::MissingPolicy::Error,
                Some("empty") => tpu::cmd::render::MissingPolicy::Empty,
                Some("leave") => tpu::cmd::render::MissingPolicy::Leave,
                Some(other) => {
                    return Err(format!(
                    "render: invalid missing policy {other:?}; expected one of \"error\", \"empty\", or \"leave\""
                )
                .into());
                }
                None => {
                    return Err(format!(
                        "render: invalid value for `missing`: expected a string, got {v}"
                    )
                    .into());
                }
            },
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
        delete_bak_if_exists(&output);
        let stamp = stamp_and_verify(std::path::Path::new(&output), config.verify_delay_ms)?;
        let result_obj = serde_json::json!({
            "reason": "x-tpu-mcp-result",
            "output": output,
            "substitutions": report.substitutions,
            "missing": report.missing,
            "mtime_epoch_ms": stamp.mtime_epoch_ms,
            "size": stamp.size,
        });
        let result_line = serde_json::to_string(&result_obj)?;
        let status_line = serde_json::to_string(&serde_json::json!({"status":"success"}))?;
        Ok(ToolResult::ok(format!(
            "{header}\n{result_line}\n{status_line}"
        )))
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_setup(args: &Value, config: &ServerConfig) -> ToolResult {
    let header = invocation_header("tpu_setup", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
        let target = match args.get("target") {
            None => None,
            Some(v) => match v.as_str() {
                Some(s) => Some(normalize_file_path(s)),
                None => {
                    return Err("`target` must be a string path when provided".into());
                }
            },
        };
        match target {
            None => {
                // Mixed mode: header line + raw markdown block content.
                let block = tpu::cmd::setup::full_block();
                Ok(ToolResult::ok(format!("{header}\n{block}")))
            }
            Some(path) => {
                let (updated, replaced) =
                    tpu::cmd::setup::inject(std::path::Path::new(&path), tpu::IoMode::Buffered)?;
                let mut result_obj = serde_json::json!({
                    "reason": "x-tpu-mcp-result",
                    "target": path,
                    "updated": updated,
                    "replaced": replaced,
                });
                if updated {
                    delete_bak_if_exists(&path);
                    let stamp =
                        stamp_and_verify(std::path::Path::new(&path), config.verify_delay_ms)?;
                    result_obj["mtime_epoch_ms"] =
                        serde_json::Value::Number(stamp.mtime_epoch_ms.into());
                    result_obj["size"] = serde_json::Value::Number(stamp.size.into());
                }
                let result_line = serde_json::to_string(&result_obj)?;
                let status_line = serde_json::to_string(&serde_json::json!({"status":"success"}))?;
                Ok(ToolResult::ok(format!(
                    "{header}\n{result_line}\n{status_line}"
                )))
            }
        }
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_doctor(args: &Value, config: &ServerConfig) -> ToolResult {
    let header = invocation_header("tpu_doctor", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
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
        if all_paths.is_empty() {
            return Err("doctor: at least one `path` or entry in `paths` is required".into());
        }

        let (fix, fix_eol) = match args.get("fix") {
            None => (tpu::cmd::doctor::DoctorFix::None, false),
            Some(v) => match v.as_str() {
                Some("none") => (tpu::cmd::doctor::DoctorFix::None, false),
                Some("peel") => (tpu::cmd::doctor::DoctorFix::Peel, false),
                Some("eol") => (tpu::cmd::doctor::DoctorFix::None, true),
                Some("all") => (tpu::cmd::doctor::DoctorFix::Peel, true),
                Some(other) => {
                    return Err(format!(
                    "invalid value for `fix`: {other:?}; expected \"none\", \"peel\", \"eol\", or \"all\""
                )
                .into());
                }
                None => {
                    return Err(
                        format!("invalid value for `fix`: expected a string, got {v}").into(),
                    );
                }
            },
        };
        // `eol`/`all` require a repository root to resolve git's expected line
        // endings; reject early with a clear message rather than silently no-op.
        let git_root = git_root_arg(args);
        if fix_eol && git_root.is_none() {
            return Err("doctor: `fix: \"eol\"`/`\"all\"` requires a `git_root` argument".into());
        }

        let on_error = match args.get("on_error") {
            None => config.default_on_error,
            Some(v) => match v.as_str() {
                Some("warn") => tpu::cmd::copy::OnError::Warn,
                Some("fail") => tpu::cmd::copy::OnError::Fail,
                Some(other) => {
                    return Err(format!(
                        "invalid value for `on_error`: {other:?}; expected \"warn\" or \"fail\""
                    )
                    .into());
                }
                None => {
                    return Err(format!(
                        "invalid value for `on_error`: expected a string, got {v}"
                    )
                    .into());
                }
            },
        };

        let opts = tpu::cmd::doctor::DoctorOptions {
            format: tpu::cmd::doctor::DoctorFormat::Json,
            fix,
            quiet: true,
            guess: false,
        };

        let path_refs: Vec<&str> = all_paths.iter().map(String::as_str).collect();
        let mut walk_warnings: Vec<String> = Vec::new();
        let mut sink: Vec<u8> = Vec::new();
        let report = tpu::cmd::doctor::run_with_policy(
            &path_refs,
            opts,
            &mut sink,
            tpu::IoMode::Buffered,
            on_error,
            &mut walk_warnings,
            git_root.as_deref(),
            fix_eol,
        )?;

        let files: Vec<Value> = report
            .issues
            .iter()
            .map(|issue| {
                let matches: Vec<Value> = issue
                    .mojibake_matches
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "byte_offset": m.byte_offset,
                            "line": m.line,
                            "col": m.col,
                            "pattern": m.pattern.name(),
                        })
                    })
                    .collect();
                let rc_matches: Vec<Value> = issue
                    .replacement_char_matches
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "byte_offset": m.byte_offset,
                            "line": m.line,
                            "col": m.col,
                            "context": m.context.as_str(),
                            "suggested": m.suggested.map(|c| format!("U+{:04X}", c as u32)),
                            "suggested_char": m.suggested.map(|c| c.to_string()),
                        })
                    })
                    .collect();
                serde_json::json!({
                    "path": issue.path.display().to_string(),
                    "encoding_detected": issue.encoding_detected,
                    "valid_in_detected_encoding": issue.valid_in_detected_encoding,
                    "mojibake_matches": matches,
                    "replacement_char_matches": rc_matches,
                    "peel_suggested": issue.peel_suggested.is_some(),
                    "repaired": issue.repaired,
                    "eol_mismatch": issue.eol_mismatch.map(|m| serde_json::json!({
                        "expected": tpu::git::line_ending_name(m.expected),
                        "actual": tpu::git::line_ending_name(m.actual),
                    })),
                    "eol_repaired": issue.eol_repaired,
                })
            })
            .collect();

        let doc = serde_json::json!({
            "reason": "x-tpu-mcp-result",
            "files": files,
            "total_files_scanned": report.total_files_scanned,
            "total_issues": report.total_issues(),
            "total_repaired": report.total_repaired,
            "walk_warnings": walk_warnings,
        });
        let result_line = serde_json::to_string(&doc)?;
        let status_line = serde_json::to_string(&serde_json::json!({"status":"success"}))?;
        Ok(ToolResult::ok(format!(
            "{header}\n{result_line}\n{status_line}"
        )))
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_stat_file(args: &Value) -> ToolResult {
    let header = invocation_header("tpu_stat_file", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
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
        let result_obj = serde_json::json!({
            "reason": "x-tpu-mcp-result",
            "size": size,
            "mtime_epoch_ms": mtime_epoch_ms,
            "created_epoch_ms": created_epoch_ms,
            "readonly": readonly,
        });
        let result_line = serde_json::to_string(&result_obj)?;
        let status_line = serde_json::to_string(&serde_json::json!({"status":"success"}))?;
        Ok(ToolResult::ok(format!(
            "{header}\n{result_line}\n{status_line}"
        )))
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
}

fn call_validate_file(args: &Value) -> ToolResult {
    let header = invocation_header("tpu_validate_file", args);
    let inner = || -> Result<ToolResult, Box<dyn std::error::Error>> {
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
        let status_line = serde_json::to_string(&serde_json::json!({"status":"success"}))?;
        Ok(ToolResult::ok(format!("{header}\n{status_line}")))
    };
    inner().unwrap_or_else(|e| ToolResult::error(&header, &e.to_string()))
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
    /// `Warn` (default) emits a warning record (included in the final JSON
    /// result) and continues with the next entry. `Fail` aborts the operation
    /// on the first error.
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

    /// When true, route every `tools/call` through a child
    /// `tpu-mcp --io-worker` process for fault isolation from Windows
    /// Defender (and similar) process-kill incidents.  Not propagated to the
    /// worker itself — the worker always runs its dispatch in-process.
    /// Default: `true` on Windows, `false` elsewhere.  Disable with
    /// `--no-io-worker` (or `TPU_MCP_NO_IO_WORKER=1`).
    pub io_worker_enabled: bool,

    /// When true, mutating tools (`tpu_write_file`, `tpu_replace_in_file`,
    /// `tpu_edit_file`, `tpu_append_file`) normalise the target file's line
    /// endings to git's expected convention — but only when the call also
    /// supplies a `git_root` and does not pass an explicit `line_ending`.
    ///
    /// Off by default (writes never silently change line endings).  Enabled
    /// by the `--eol-normalize` flag or `TPU_EOL_NORMALIZE=1`, which the VS
    /// Code extension forwards from its `tpu-mcp.normalizeLineEndings` setting.
    pub eol_normalize: bool,
}

/// How much per-entry detail tree-walking tools should include in their
/// output.  `tpu_copy_file` includes per-entry diagnostics in a JSON result;
/// `tpu_find` appends them as plain text.
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
            io_worker_enabled: cfg!(windows),
            eol_normalize: false,
        }
    }
}

impl ServerConfig {
    /// Encode the config as a JSON object suitable for transport to a child
    /// `tpu-mcp --io-worker` process.  Enum fields are serialised as the
    /// short string values accepted by the matching CLI flags.
    pub fn to_wire(&self) -> Value {
        let on_error = match self.default_on_error {
            tpu::cmd::copy::OnError::Warn => "warn",
            tpu::cmd::copy::OnError::Fail => "fail",
        };
        let progress_detail = match self.progress_detail {
            ProgressDetail::EachFile => "each-file",
            ProgressDetail::Summary => "summary",
        };
        serde_json::json!({
            "verify_delay_ms": self.verify_delay_ms,
            "trace": self.trace,
            "default_on_error": on_error,
            "progress_detail": progress_detail,
            "eol_normalize": self.eol_normalize,
        })
    }

    /// Decode a `ServerConfig` previously produced by [`Self::to_wire`].
    /// Missing fields fall back to defaults so a worker can tolerate a
    /// slightly newer parent process.
    pub fn from_wire(v: &Value) -> Result<Self, String> {
        let d = Self::default();
        let verify_delay_ms = v
            .get("verify_delay_ms")
            .and_then(|x| x.as_u64())
            .unwrap_or(d.verify_delay_ms);
        let trace = v.get("trace").and_then(|x| x.as_bool()).unwrap_or(d.trace);
        let default_on_error = match v.get("default_on_error").and_then(|x| x.as_str()) {
            Some("warn") | None => tpu::cmd::copy::OnError::Warn,
            Some("fail") => tpu::cmd::copy::OnError::Fail,
            Some(other) => return Err(format!("unknown default_on_error: {other:?}")),
        };
        let progress_detail = match v.get("progress_detail").and_then(|x| x.as_str()) {
            Some("each-file") | Some("each_file") | None => ProgressDetail::EachFile,
            Some("summary") => ProgressDetail::Summary,
            Some(other) => return Err(format!("unknown progress_detail: {other:?}")),
        };
        let eol_normalize = v
            .get("eol_normalize")
            .and_then(|x| x.as_bool())
            .unwrap_or(d.eol_normalize);
        Ok(ServerConfig {
            verify_delay_ms,
            trace,
            default_on_error,
            progress_detail,
            eol_normalize,
            // Always false inside the worker — the worker is the leaf that
            // actually performs file I/O.  Routing through another worker
            // would loop.
            io_worker_enabled: false,
        })
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
///   - `\n`  → LF   (`0x0A`)
///   - `\r`  → CR   (`0x0D`)
///   - `\t`  → TAB  (`0x09`)
///   - `\\` → `\`
///
/// All other `\X` sequences are passed through unchanged — both the backslash
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
        // Fast path — no backslashes at all; nothing to do.
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

/// Resolve a text payload argument, honouring an optional `{key}_format`
/// escape-hazard-free channel (see issue #53).
///
/// JSON string transport means a literal `\n` (single backslash) an agent
/// intends to land as two bytes in the file is indistinguishable, once
/// decoded, from an agent-intended real newline — the ambiguity is resolved
/// (wrongly, from the caller's point of view) before tpu ever sees the
/// string. `{key}_format: "base64"` (or `"hex"`) sidesteps this: the wire
/// value contains no backslashes at all, so JSON decoding cannot introduce
/// or remove escape sequences, and the decoded bytes are exactly what the
/// caller intended, byte for byte.
///
/// When `{key}_format` is absent, falls back to the plain JSON-string value
/// (unchanged behaviour). Either way, CRLF/CR occurring in the resolved text
/// is normalised to LF, matching every other text-content argument.
fn decode_content_arg(args: &Value, key: &str) -> Result<String, Box<dyn std::error::Error>> {
    let format_key = format!("{key}_format");
    let text = match args.get(&format_key).and_then(|v| v.as_str()) {
        Some(fmt_str) => {
            let fmt = parse_data_format(fmt_str)?;
            let raw = require_str(args, key)?;
            let bytes =
                tpu::data_format::decode(&fmt, raw).map_err(|e| format!("{format_key}: {e}"))?;
            String::from_utf8(bytes)
                .map_err(|e| format!("{format_key}: decoded bytes are not valid UTF-8: {e}"))?
        }
        None => require_str(args, key)?.to_owned(),
    };
    Ok(normalize_to_lf(&text).into_owned())
}

/// Resolve `tpu_replace_in_file`'s `pattern` argument, honouring the optional
/// `pattern_format` escape-hazard-free channel (see [`decode_content_arg`]).
/// Unlike content/replacement, `pattern` is never LF-normalised here — it is
/// matched against the file's already LF-normalised view exactly as today.
fn decode_pattern_arg(args: &Value, key: &str) -> Result<String, Box<dyn std::error::Error>> {
    let format_key = format!("{key}_format");
    match args.get(&format_key).and_then(|v| v.as_str()) {
        Some(fmt_str) => {
            let fmt = parse_data_format(fmt_str)?;
            let raw = require_str(args, key)?;
            let bytes =
                tpu::data_format::decode(&fmt, raw).map_err(|e| format!("{format_key}: {e}"))?;
            String::from_utf8(bytes)
                .map_err(|e| format!("{format_key}: decoded bytes are not valid UTF-8: {e}").into())
        }
        None => Ok(require_str(args, key)?.to_owned()),
    }
}

/// Resolve `tpu_replace_in_file`'s `replacement` argument.
///
/// When `replacement_format` is set, the decoded bytes are taken literally:
/// this is the whole point of the escape-hazard-free channel, so tpu's own
/// backslash-escape convenience decoding ([`unescape_replacement`]) is
/// skipped — it would otherwise reinterpret bytes the caller has already
/// specified exactly. Without `replacement_format`, behaviour is unchanged:
/// the plain JSON string is backslash-unescaped, then LF-normalised.
fn decode_replacement_arg(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    match args.get("replacement_format").and_then(|v| v.as_str()) {
        Some(fmt_str) => {
            let fmt = parse_data_format(fmt_str)?;
            let raw = require_str(args, "replacement")?;
            let bytes = tpu::data_format::decode(&fmt, raw)
                .map_err(|e| format!("replacement_format: {e}"))?;
            let text = String::from_utf8(bytes).map_err(|e| {
                format!("replacement_format: decoded bytes are not valid UTF-8: {e}")
            })?;
            Ok(normalize_to_lf(&text).into_owned())
        }
        None => {
            let replacement_raw = require_str(args, "replacement")?;
            let replacement_unescaped = unescape_replacement(replacement_raw);
            Ok(normalize_to_lf(&replacement_unescaped).into_owned())
        }
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

/// Reject the removed `fixed_strings` argument with a migration error instead
/// of silently ignoring it (ad-hoc JSON arg parsing otherwise drops unknown
/// keys, which would flip a caller's intended `fixed_strings:false` regex
/// search into today's literal-by-default matching without any signal).
fn reject_removed_fixed_strings_arg(args: &Value) -> Result<(), Box<dyn std::error::Error>> {
    if args.get("fixed_strings").is_some() {
        return Err(
            "the 'fixed_strings' argument was removed: matching is now literal \
             by default, and regex is opt-in via \"regex\": true (the inverse of the old \
             'fixed_strings' meaning)"
                .into(),
        );
    }
    Ok(())
}

/// Build a [`tpu::mojibake::WritePolicy`] from a tool-call's JSON args.
///
/// Recognises the `allow_mojibake` boolean (default `false`); when `true`,
/// the write-time mojibake guard is disabled for that call.  Mirrors the
/// CLI's `--allow-mojibake` flag.
/// Returns `"\n"` when `s` does not end with a newline, otherwise `""`.
///
/// Diff output originates from an external writer and is not guaranteed to end
/// with a newline. Calling this before concatenating a JSON status line ensures
/// the result is valid NDJSON (one JSON object per line).
fn diff_separator(s: &str) -> &'static str {
    if s.ends_with('\n') { "" } else { "\n" }
}

/// Maximum bytes of a single echoed line before it's truncated with a
/// marker. `echo_max_lines` only bounds the number of lines, so a handful
/// of individually-huge lines (minified JSON, a base64 blob, ...) could
/// otherwise still produce a very large response despite passing that gate.
const MAX_ECHO_LINE_BYTES: usize = 500;

/// Render `tpu_replace_in_file`'s default changed-region echo from cheap
/// per-match [`tpu::cmd::replace::ChangedRegion`] data (see its doc comment)
/// -- deliberately NOT a full unified diff, since that would require the
/// whole-file clone this mechanism exists to avoid. Each region renders as a
/// unified-diff-style hunk header (for familiar tooling/eyeballs) followed
/// only by the NEW text -- the old text isn't cheaply available without the
/// full-file clone, so it isn't shown here; pass diff:true for that. Any
/// single rendered line longer than [`MAX_ECHO_LINE_BYTES`] is truncated
/// with a marker (see that constant's doc comment).
fn render_changed_regions(regions: &[tpu::cmd::replace::ChangedRegion]) -> String {
    let mut out = String::new();
    for r in regions {
        let old_count = r.end_line - r.start_line + 1;
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            r.start_line, old_count, r.start_line, r.new_line_count
        ));
        if !r.new_text.is_empty() {
            let mut lines: Vec<&str> = r.new_text.split('\n').collect();
            // A trailing '\n' in new_text produces one spurious empty
            // element; drop only that, not any intentional blank lines.
            if lines.last() == Some(&"") {
                lines.pop();
            }
            for line in lines {
                out.push('+');
                if line.len() > MAX_ECHO_LINE_BYTES {
                    let mut boundary = MAX_ECHO_LINE_BYTES;
                    while !line.is_char_boundary(boundary) {
                        boundary -= 1;
                    }
                    out.push_str(&line[..boundary]);
                    out.push_str(&format!("... [truncated, {} bytes total]", line.len()));
                } else {
                    out.push_str(line);
                }
                out.push('\n');
            }
        }
    }
    out
}

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
///
/// ## Trust boundary
///
/// This function performs **no workspace-root confinement**: the returned
/// path is whatever the caller asked for, and the tool will read or write any
/// path the host user can access.  That is intentional for a general-purpose
/// local file editor, but because the call surface is an LLM tool, a
/// prompt-injected agent could be steered to touch sensitive paths (e.g.
/// `~/.ssh/...`).  The trust boundary is therefore the same as the user's own
/// shell — operators who need confinement should run the server under an
/// account or sandbox with appropriately scoped filesystem permissions.  See
/// the "Security / trust boundary" section of the tpu-mcp README.
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
///
/// Bytes are collected first and then interpreted as UTF-8 so that
/// multi-byte sequences round-trip correctly. For example, `%C3%A9`
/// decodes to the two-byte UTF-8 sequence for U+00E9 (e-acute) rather
/// than two separate Latin-1 scalars (the bytes 0xC3 and 0xA9 treated
/// independently).
fn percent_decode_path(s: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(hi), Some(lo)) = (hex_nibble(b[i + 1]), hex_nibble(b[i + 2]))
        {
            bytes.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        bytes.push(b[i]);
        i += 1;
    }
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
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

    // -- git_root argument normalization --

    #[test]
    fn git_root_arg_accepts_file_uri() {
        // VS Code integrations often pass `git_root` as a `file://` URI with
        // percent-encoding; it must resolve to the same path as a plain
        // filesystem path (regression for PR #41 review r3470390782).
        let args = serde_json::json!({ "git_root": "file:///z%3A/src4/repo" });
        assert_eq!(
            git_root_arg(&args),
            Some(std::path::PathBuf::from("z:/src4/repo"))
        );
    }

    #[test]
    fn git_root_arg_accepts_plain_path() {
        let args = serde_json::json!({ "git_root": "/home/user/repo" });
        assert_eq!(
            git_root_arg(&args),
            Some(std::path::PathBuf::from("/home/user/repo"))
        );
    }

    #[test]
    fn git_root_arg_absent_or_empty_is_none() {
        assert_eq!(git_root_arg(&serde_json::json!({})), None);
        assert_eq!(git_root_arg(&serde_json::json!({ "git_root": "" })), None);
    }

    // -- EOL advisory is line-oriented: never prefixed onto byte output --

    /// Build a repo whose `.gitattributes` forces `eol=lf`, write `name` with
    /// CRLF endings (a mismatch git would flag), and return `(dir, file path)`.
    fn repo_with_crlf_mismatch(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        gix::init(dir.path()).expect("git init");
        std::fs::write(dir.path().join(".gitattributes"), "*.txt text eol=lf\n")
            .expect("write .gitattributes");
        let file = dir.path().join(name);
        std::fs::write(&file, b"alpha\r\nbravo\r\ncharlie\r\n").expect("write file");
        (dir, file)
    }

    #[test]
    fn read_head_line_mode_prepends_eol_note() {
        let (dir, file) = repo_with_crlf_mismatch("a.txt");
        let args = serde_json::json!({
            "file": file.to_string_lossy(),
            "lines": 2,
            "git_root": dir.path().to_string_lossy(),
        });
        let res = call_read_head(&args);
        assert!(!res.is_error);
        assert!(
            res.text.contains("note:") && res.text.contains("line endings"),
            "line-mode head should carry the EOL advisory: {}",
            res.text
        );
    }

    #[test]
    fn read_head_byte_mode_omits_eol_note() {
        let (dir, file) = repo_with_crlf_mismatch("a.txt");
        let args = serde_json::json!({
            "file": file.to_string_lossy(),
            "bytes": 8,
            "git_root": dir.path().to_string_lossy(),
        });
        let res = call_read_head(&args);
        assert!(!res.is_error);
        assert!(
            !res.text.contains("note:"),
            "byte-mode head must not prefix a line-oriented EOL note: {}",
            res.text
        );
    }

    #[test]
    fn read_tail_byte_mode_omits_eol_note() {
        let (dir, file) = repo_with_crlf_mismatch("a.txt");
        let args = serde_json::json!({
            "file": file.to_string_lossy(),
            "bytes": 8,
            "git_root": dir.path().to_string_lossy(),
        });
        let res = call_read_tail(&args);
        assert!(!res.is_error);
        assert!(
            !res.text.contains("note:"),
            "byte-mode tail must not prefix a line-oriented EOL note: {}",
            res.text
        );
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
    fn percent_decode_utf8_multibyte() {
        // U+00E9 (e-acute) is two UTF-8 bytes: 0xC3 0xA9.  Each %XX byte
        // must not be treated as a Latin-1 scalar (which would produce two
        // separate code points instead of the single e-acute character).
        assert_eq!(percent_decode_path("%C3%A9"), "\u{e9}");
        assert_eq!(percent_decode_path("/path/caf%C3%A9"), "/path/caf\u{e9}");
        // Japanese: ファイル — 3 x 3-byte UTF-8 sequences
        assert_eq!(
            percent_decode_path("%E3%83%95%E3%82%A1%E3%82%A4%E3%83%AB"),
            "\u{30D5}\u{30A1}\u{30A4}\u{30EB}"
        );
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
        // No backslash at all — fast path returns owned copy unchanged.
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
        // $1 and $name must survive unmodified — no backslash involved.
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
        // \x, \$, \q etc. are not recognised — both chars pass through.
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

    /// Parse the last JSON object line from NDJSON output (the status/result trailer).
    fn last_json_line(output: &str) -> serde_json::Value {
        output
            .trim()
            .lines()
            .rev()
            .find_map(|l| {
                let l = l.trim();
                if l.is_empty() {
                    return None;
                }
                serde_json::from_str::<serde_json::Value>(l).ok()
            })
            .unwrap_or(serde_json::Value::Null)
    }

    /// Find the `{"reason":"x-tpu-mcp-result",...}` line in NDJSON output.
    fn ndjson_result_line(output: &str) -> serde_json::Value {
        for line in output.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("reason").and_then(|r| r.as_str()) == Some("x-tpu-mcp-result") {
                    return v;
                }
            }
        }
        serde_json::Value::Null
    }

    /// Zero-delay wrapper so existing tests call the real `call()` without
    /// needing to supply a `ServerConfig` argument explicitly.
    fn call(name: &str, args: &serde_json::Value) -> Result<String, Box<dyn std::error::Error>> {
        let tr = super::call(
            name,
            args,
            &ServerConfig {
                verify_delay_ms: 0,
                trace: false,
                default_on_error: tpu::cmd::copy::OnError::Warn,
                progress_detail: ProgressDetail::EachFile,
                io_worker_enabled: false,
                eol_normalize: false,
            },
        )?;
        if tr.is_error {
            return Err(tr.text.into());
        }
        Ok(tr.text)
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

        // tpu find emits matching lines as plain text with JSON header/trailer.
        // Filter out JSON lines to get only content lines.
        let content_lines: Vec<&str> = result
            .lines()
            .filter(|l| serde_json::from_str::<serde_json::Value>(l.trim()).is_err())
            .filter(|l| !l.trim().is_empty())
            .collect();
        assert_eq!(
            content_lines.len(),
            2,
            "expected 2 matching lines; got: {result:?}"
        );
        assert!(
            content_lines[0].contains("alpha fox here"),
            "line 0 must contain 'alpha fox here'; got: {:?}",
            content_lines[0]
        );
        assert!(
            content_lines[1].contains("gamma fox again"),
            "line 1 must contain 'gamma fox again'; got: {:?}",
            content_lines[1]
        );

        // tpu_find with no matches must return Ok (not Err) and have no content lines.
        let args_no_match = serde_json::json!({
            "pattern": "zzz_no_match_zzz",
            "path": f.to_str().expect("temp path must be valid UTF-8"),
        });
        let no_match =
            call("tpu_find", &args_no_match).expect("tpu_find with no matches must be Ok, not Err");
        let content_no_match: Vec<&str> = no_match
            .lines()
            .filter(|l| serde_json::from_str::<serde_json::Value>(l.trim()).is_err())
            .filter(|l| !l.trim().is_empty())
            .collect();
        assert!(
            content_no_match.is_empty(),
            "no-match result must have no content lines; got: {no_match:?}"
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

    // -- escape-hazard *_format channel (issue #53) -----------------------------

    /// EH-IT-1: `tpu_write_file` with `content_format: "base64"` writes the
    /// exact decoded bytes, bypassing JSON-escape ambiguity entirely.
    /// Regression for issue #53: a literal two-character `\n` (backslash + n)
    /// must survive verbatim rather than being decoded to a real newline.
    #[test]
    fn eh_it_1_write_file_content_format_base64_literal_backslash_n() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("content_format_base64.txt");

        // Intended file content: the literal two characters \ and n (not a newline).
        let intended = "line one\\nline two";
        let encoded = tpu::data_format::encode_base64(intended.as_bytes());

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "content": encoded,
            "content_format": "base64",
        });
        call("tpu_write_file", &args).expect("tpu_write_file must succeed");

        let written = fs::read_to_string(&f).unwrap();
        assert_eq!(
            written, intended,
            "base64 channel must preserve literal backslash+n verbatim; got: {written:?}"
        );

        drop(dir);
    }

    /// EH-IT-2: `tpu_append_file` with `content_format: "base64"`.
    #[test]
    fn eh_it_2_append_file_content_format_base64() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("append_base64.txt");
        fs::write(&f, "existing\n").unwrap();

        let intended = "appended\\ttabbed";
        let encoded = tpu::data_format::encode_base64(intended.as_bytes());
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "content": encoded,
            "content_format": "base64",
        });
        call("tpu_append_file", &args).expect("tpu_append_file must succeed");

        let written = fs::read_to_string(&f).unwrap();
        assert_eq!(written, format!("existing\n{intended}"));

        drop(dir);
    }

    /// EH-IT-3: `tpu_create_file` with `content_format: "base64"`.
    #[test]
    fn eh_it_3_create_file_content_format_base64() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("create_base64.txt");

        let intended = "brand new \\n not a newline";
        let encoded = tpu::data_format::encode_base64(intended.as_bytes());
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "content": encoded,
            "content_format": "base64",
        });
        call("tpu_create_file", &args).expect("tpu_create_file must succeed");

        let written = fs::read_to_string(&f).unwrap();
        assert_eq!(written, intended);

        drop(dir);
    }

    /// EH-IT-4: `tpu_replace_in_file` with `pattern_format: "base64"` finds a
    /// literal search target containing backslash+n without needing regex or
    /// JSON double-escaping.
    #[test]
    fn eh_it_4_replace_pattern_format_base64_literal_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("pattern_base64.txt");
        fs::write(&f, "prefix line one\\nline two suffix\n").unwrap();

        let pattern = "one\\nline";
        let encoded = tpu::data_format::encode_base64(pattern.as_bytes());
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": encoded,
            "pattern_format": "base64",
            "replacement": "ONE-LINE",
        });
        call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        let written = fs::read_to_string(&f).unwrap();
        assert_eq!(written, "prefix line ONE-LINE two suffix\n");

        drop(dir);
    }

    /// EH-IT-5: `tpu_replace_in_file` with `replacement_format: "base64"`
    /// writes the exact decoded bytes verbatim — no backslash-escape
    /// convenience decoding is applied on top (unlike the plain-text path).
    #[test]
    fn eh_it_5_replace_replacement_format_base64_bypasses_unescape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("replacement_base64.txt");
        fs::write(&f, "TARGET\n").unwrap();

        // Intended replacement literally contains \n (backslash + n), which
        // the plain-text path's unescape_replacement would turn into a real
        // newline. The base64 channel must not apply that decoding.
        let intended = "before\\nafter";
        let encoded = tpu::data_format::encode_base64(intended.as_bytes());
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": "TARGET",
            "replacement": encoded,
            "replacement_format": "base64",
        });
        call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        let written = fs::read_to_string(&f).unwrap();
        assert_eq!(
            written,
            format!("{intended}\n"),
            "replacement_format:base64 must not apply backslash-escape decoding; got: {written:?}"
        );

        drop(dir);
    }

    /// EH-IT-6: `tpu_edit_file`'s `data_format` works in TEXT mode (not just
    /// binary mode), bypassing the same JSON-escape hazard for an op's `data`.
    #[test]
    fn eh_it_6_edit_file_data_format_base64_in_text_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("edit_base64.txt");
        fs::write(&f, "one\ntwo\nthree\n").unwrap();

        let intended = "inserted\\nliteral";
        let encoded = tpu::data_format::encode_base64(intended.as_bytes());
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "ops": [
                { "op": "insert", "offset": "2", "data": encoded, "data_format": "base64" }
            ],
        });
        call("tpu_edit_file", &args).expect("tpu_edit_file must succeed");

        let written = fs::read_to_string(&f).unwrap();
        assert!(
            written.contains(intended),
            "text-mode edit_file data_format:base64 must preserve literal bytes; got: {written:?}"
        );

        drop(dir);
    }

    /// EH-IT-7: an invalid base64 payload in `content_format` produces a
    /// descriptive error rather than silently writing garbage, and the file
    /// is not created.
    #[test]
    fn eh_it_7_content_format_invalid_base64_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("bad_base64.txt");

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "content": "not valid base64!!!",
            "content_format": "base64",
        });
        let out = call("tpu_write_file", &args);
        assert!(out.is_err(), "invalid base64 must produce an error result");
        assert!(!f.exists(), "file must not be created on decode failure");

        drop(dir);
    }

    // -- default changed-region echo (issue #53 mitigation 2) -------------------

    /// ER-IT-1: `tpu_replace_in_file` on a small change (well under
    /// echo_max_lines) automatically includes a compact unified diff in the
    /// response, with no `diff:true` needed, plus a `changed_lines` count.
    #[test]
    fn er_it_1_replace_small_change_echoed_by_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("small_change.txt");
        fs::write(&f, "hello world\n").unwrap();

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": "world",
            "replacement": "there",
        });
        let out = call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        assert!(
            out.contains("@@"),
            "small change must auto-echo a unified diff hunk; got: {out:?}"
        );
        let v = last_json_line(&out);
        assert_eq!(
            v["changed_lines"].as_u64().unwrap(),
            2,
            "one line replaced -> 1 removed + 1 added; got: {out:?}"
        );
        assert!(
            v.get("diff_omitted").is_none(),
            "small change must not be marked as omitted; got: {out:?}"
        );

        drop(dir);
    }

    /// ER-IT-2: `tpu_replace_in_file` on a change larger than the default
    /// `echo_max_lines` (5) omits the diff by default, reporting
    /// `changed_lines` and `diff_omitted:true` instead.
    #[test]
    fn er_it_2_replace_large_change_omitted_by_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("large_change.txt");
        let old = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n";
        let new = "L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\n";
        fs::write(&f, old).unwrap();

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": old,
            "replacement": new,
        });
        let out = call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        assert!(
            !out.contains("@@"),
            "large change must not auto-echo a diff by default; got: {out:?}"
        );
        let v = last_json_line(&out);
        let changed_lines = v["changed_lines"].as_u64().unwrap();
        assert!(
            changed_lines > 5,
            "expected more than 5 changed lines; got: {changed_lines} ({out:?})"
        );
        assert_eq!(
            v["diff_omitted"], true,
            "large change must be marked as omitted; got: {out:?}"
        );

        drop(dir);
    }

    /// ER-IT-3: raising `echo_max_lines` echoes a change that would
    /// otherwise be omitted under the default threshold.
    #[test]
    fn er_it_3_replace_echo_max_lines_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("large_change_override.txt");
        let old = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n";
        let new = "L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\n";
        fs::write(&f, old).unwrap();

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": old,
            "replacement": new,
            "echo_max_lines": 30,
        });
        let out = call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        assert!(
            out.contains("@@"),
            "raised echo_max_lines must allow the diff to be echoed; got: {out:?}"
        );
        let v = last_json_line(&out);
        assert!(
            v.get("diff_omitted").is_none(),
            "diff must not be marked omitted once echo_max_lines covers it; got: {out:?}"
        );

        drop(dir);
    }

    /// ER-IT-4: explicit `diff:true` always shows the full diff regardless
    /// of size, unaffected by `echo_max_lines`.
    #[test]
    fn er_it_4_replace_diff_true_ignores_echo_max_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("large_change_explicit_diff.txt");
        let old = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n";
        let new = "L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\n";
        fs::write(&f, old).unwrap();

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": old,
            "replacement": new,
            "diff": true,
        });
        let out = call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        assert!(
            out.contains("@@"),
            "diff:true must show the diff regardless of size; got: {out:?}"
        );
        let v = last_json_line(&out);
        assert!(
            v.get("diff_omitted").is_none(),
            "diff:true must never mark the diff as omitted; got: {out:?}"
        );

        drop(dir);
    }

    /// ER-IT-5: a single-line replacement that is itself very long (e.g. a
    /// minified JSON blob) is truncated with a marker in the default echo,
    /// even though `changed_lines` (2: one old line + one new line) is well
    /// under `echo_max_lines` -- the line-count gate alone can't bound an
    /// individually-huge line.
    #[test]
    fn er_it_5_replace_default_echo_truncates_long_single_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("huge_line.txt");
        fs::write(&f, "TARGET\n").unwrap();

        let huge = "x".repeat(2000);
        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "pattern": "TARGET",
            "replacement": huge,
        });
        let out = call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");

        assert!(
            out.contains("[truncated, 2000 bytes total]"),
            "a huge single line must be truncated with a marker; got: {} chars",
            out.len()
        );
        assert!(
            !out.contains(&huge),
            "the full untruncated huge line must not appear verbatim in the response"
        );
        let v = last_json_line(&out);
        assert_eq!(
            v["changed_lines"], 2,
            "changed_lines must still reflect 1 old + 1 new line, not byte count"
        );
        assert!(
            v.get("diff_omitted").is_none(),
            "small line count must not be marked as omitted just because a line is long"
        );

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
            "regex": true,
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
        let v = last_json_line(&result);
        assert!(
            v["mtime_epoch_ms"].as_u64().unwrap_or(0) > 0,
            "response must contain mtime_epoch_ms; got: {result:?}"
        );
        assert!(
            v.get("size").is_some(),
            "response must contain size; got: {result:?}"
        );
        assert_eq!(v["size"], 12, "size must be 12; got: {result:?}");

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
        });

        let result = call("tpu_replace_in_file", &args).expect("tpu_replace_in_file must succeed");
        let v = last_json_line(&result);
        assert!(
            v["mtime_epoch_ms"].as_u64().unwrap_or(0) > 0,
            "response must contain mtime_epoch_ms; got: {result:?}"
        );
        assert_eq!(v["size"], 4, "size must be 4; got: {result:?}");

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
        let v = last_json_line(&result);
        assert!(
            v["mtime_epoch_ms"].as_u64().unwrap_or(0) > 0,
            "response must contain mtime_epoch_ms; got: {result:?}"
        );
        assert_eq!(v["size"], 12, "size must be 12; got: {result:?}");

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
        let v = last_json_line(&result);
        assert!(
            v["mtime_epoch_ms"].as_u64().unwrap_or(0) > 0,
            "response must contain mtime_epoch_ms; got: {result:?}"
        );
        assert_eq!(v["size"], 4, "size must be 4 after delete; got: {result:?}");

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

        let v = ndjson_result_line(&result);
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

        // Extract mtime_epoch_ms from the last JSON line of the NDJSON response.
        let write_v = last_json_line(&write_result);
        let mtime_write: u64 = write_v["mtime_epoch_ms"]
            .as_u64()
            .expect("write result must contain mtime_epoch_ms");

        let stat_args = serde_json::json!({ "file": &path_str });
        let stat_result = call("tpu_stat_file", &stat_args).expect("tpu_stat_file must succeed");
        let stat = ndjson_result_line(&stat_result);
        let mtime_stat = stat["mtime_epoch_ms"].as_u64().unwrap();

        // With verify_delay_ms=0 the stamp is not set explicitly; both values
        // come from OS metadata so they should agree within 2 seconds.
        assert!(
            mtime_write.abs_diff(mtime_stat) < 2_000,
            "write mtime {mtime_write} and stat mtime {mtime_stat} must be within 2s"
        );

        drop(dir);
    }

    // -- call_count_file -------------------------------------------------------

    /// CF-IT-1: `tpu_count_file` with no metric flags must return all four
    /// standard metrics (`lines`, `words`, `chars`, `bytes`) at the top level
    /// of the `x-tpu-mcp-result` object, plus the always-on stats fields
    /// (`encoding`, `bom`, `line_ending`).
    ///
    /// Regression for: when all flags defaulted to `false`, `standard_metric_names`
    /// was empty so every metric emitted by `count::run` fell through the routing
    /// guard and was silently dropped, producing an empty result object.
    #[test]
    fn cf_it_1_count_file_no_flags_returns_all_four_metrics() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("count_default.txt");
        // 2 lines, 3 words, known byte/char content.
        fs::write(&f, "hello world\nline two\n").unwrap();

        let args = serde_json::json!({ "file": f.to_str().unwrap() });
        let out = call("tpu_count_file", &args).expect("tpu_count_file must succeed");

        let result = ndjson_result_line(&out);
        assert_eq!(
            result["reason"], "x-tpu-mcp-result",
            "result line must be present; full output: {out:?}"
        );

        for metric in ["lines", "words", "chars", "bytes"] {
            assert!(
                result.get(metric).and_then(|v| v.as_u64()).is_some(),
                "result must contain numeric '{metric}'; got: {result:?}"
            );
        }
        assert_eq!(result["lines"].as_u64().unwrap(), 2, "lines count mismatch");

        // Stats are always present regardless of the stats flag.
        for stats_key in ["encoding", "bom", "line_ending"] {
            assert!(
                result.get(stats_key).is_some(),
                "stats field '{stats_key}' must always be present; got: {result:?}"
            );
        }

        drop(dir);
    }

    /// CF-IT-2: `tpu_count_file` with only `lines: true` returns just `lines`
    /// at the top level; `words`, `chars`, `bytes` are absent.
    #[test]
    fn cf_it_2_count_file_explicit_lines_flag_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("count_lines.txt");
        fs::write(&f, "one\ntwo\nthree\n").unwrap();

        let args = serde_json::json!({ "file": f.to_str().unwrap(), "lines": true });
        let out = call("tpu_count_file", &args).expect("tpu_count_file must succeed");

        let result = ndjson_result_line(&out);
        assert_eq!(result["lines"].as_u64().unwrap(), 3, "explicit lines count");
        for absent in ["words", "chars", "bytes"] {
            assert!(
                result.get(absent).is_none(),
                "'{absent}' must be absent when not requested; got: {result:?}"
            );
        }

        drop(dir);
    }

    /// CF-IT-3: when a pattern is given a label that collides with a standard
    /// metric name (e.g. `"lines"`), the standard metric must survive at the
    /// top level and the pattern result must go into `result["patterns"]["lines"]`.
    /// Regression for: the previous routing used `pattern_label_set.contains(metric)`
    /// which would route the real `lines` count into `patterns` on a collision.
    #[test]
    fn cf_it_3_count_file_pattern_label_collision_with_standard_metric() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("count_collision.txt");
        fs::write(&f, "one\ntwo\nthree\n").unwrap();

        let args = serde_json::json!({
            "file": f.to_str().unwrap(),
            "lines": true,
            "patterns": [{ "pattern": "two", "label": "lines" }],
        });
        let out = call("tpu_count_file", &args).expect("tpu_count_file must succeed");

        let result = ndjson_result_line(&out);
        // Standard lines count must be at the top level and correct.
        assert_eq!(
            result["lines"].as_u64().unwrap(),
            3,
            "standard lines count must survive label collision; got: {result:?}"
        );
        // The colliding pattern result must land in result["patterns"]["lines"].
        assert_eq!(
            result["patterns"]["lines"].as_u64().unwrap(),
            1,
            "pattern matching 'two' with label 'lines' must be in patterns sub-object; got: {result:?}"
        );

        drop(dir);
    }

    /// CF-IT-4: the `stats` argument is a no-op — `encoding`, `bom`, and
    /// `line_ending` must be present whether `stats` is true, false, or absent.
    #[test]
    fn cf_it_4_count_file_stats_always_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("count_stats.txt");
        fs::write(&f, "hello\n").unwrap();

        for stats_flag in [serde_json::json!(true), serde_json::json!(false)] {
            let args = serde_json::json!({ "file": f.to_str().unwrap(), "stats": stats_flag });
            let out = call("tpu_count_file", &args).unwrap_or_else(|e| {
                panic!("tpu_count_file must succeed (stats={stats_flag}): {e}")
            });
            let result = ndjson_result_line(&out);
            for key in ["encoding", "bom", "line_ending"] {
                assert!(
                    result.get(key).is_some(),
                    "'{key}' must be present when stats={stats_flag}; got: {result:?}"
                );
            }
        }

        // Also test with no stats arg at all.
        let args = serde_json::json!({ "file": f.to_str().unwrap() });
        let out =
            call("tpu_count_file", &args).expect("tpu_count_file must succeed (no stats arg)");
        let result = ndjson_result_line(&out);
        for key in ["encoding", "bom", "line_ending"] {
            assert!(
                result.get(key).is_some(),
                "'{key}' must be present with no stats arg; got: {result:?}"
            );
        }

        drop(dir);
    }
}
