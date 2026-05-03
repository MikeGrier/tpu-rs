// Copyright (c) 2026, Michael Grier

//! NDJSON message types for `--message-format=json`.
//!
//! Each variant of [`Msg`] serialises to one JSON object whose `reason` field
//! identifies the type.  The format mirrors Cargo's `--message-format=json`:
//! callers write one JSON line per message to stdout; stderr is silent.
//!
//! **Stability guarantee**: field names and `reason` values are part of the
//! public interface visible to the MCP server and any downstream tooling.
//! Changing them is a breaking change — add a design note before doing so.

use serde::Serialize;

// ── MessageFormat ─────────────────────────────────────────────────────────────

/// Selects whether output is human-readable or machine-readable JSON (NDJSON).
///
/// Parsed from the `--message-format` CLI flag and forwarded to [`Shell`].
///
/// [`Shell`]: crate::shell::Shell
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageFormat {
    /// Coloured, human-readable messages on stderr (default).
    #[default]
    Human,
    /// Newline-delimited JSON objects on stdout; stderr is silent.
    Json,
}

// ── Msg ───────────────────────────────────────────────────────────────────────

/// A single NDJSON message line.
///
/// Each variant serialises to a JSON object with a `reason` field:
///
/// | Variant      | `reason`    | Additional fields                              |
/// |--------------|-------------|------------------------------------------------|
/// | `Status`     | `status`    | `verb`, `message`                              |
/// | `Warning`    | `warning`   | `message`                                      |
/// | `Error`      | `error`     | `message`                                      |
/// | `Data`       | `data`      | `subcommand`, `encoding`, `content`            |
/// | `Diff`       | `diff`      | `subcommand`, `content`                        |
/// | `Finished`   | `finished`  | `success`                                      |
///
/// **`encoding` values for `Data`:**
///
/// - `"text"` — UTF-8 with LF endings (text-mode `read`/`readex` output).
/// - `"bytes-base64"` — raw bytes encoded as RFC 4648 Base64.
/// - `"bytes-hex"` — uppercase hex pairs separated by `-` (e.g. `"4D-5A"`).
/// - `"bytes-encoded"` — tpu escape codec (`\xHH` for non-printable bytes).
#[allow(dead_code)]
#[derive(Debug, Serialize)]
#[serde(tag = "reason", rename_all = "lowercase")]
pub enum Msg<'a> {
    /// A human-readable status update.
    ///
    /// Example: `{"reason":"status","verb":"replace","message":"file.txt: 3 replacements"}`
    Status {
        /// Short verb describing the operation (e.g. `"replace"`, `"read"`).
        verb: &'a str,
        /// Human-readable message body.
        message: &'a str,
    },

    /// A non-fatal diagnostic.
    ///
    /// Example: `{"reason":"warning","message":"file has no trailing newline"}`
    Warning {
        /// Warning text.
        message: &'a str,
    },

    /// A fatal diagnostic.  Always followed by `Finished { success: false }`.
    ///
    /// Example: `{"reason":"error","message":"file not found: foo.txt"}`
    Error {
        /// Error text.
        message: &'a str,
    },

    /// Payload content produced by a read subcommand.
    ///
    /// Example: `{"reason":"data","subcommand":"read","encoding":"text","content":"hello\n"}`
    Data {
        /// Subcommand that produced this payload (`"read"`, `"readex"`, …).
        subcommand: &'a str,
        /// How `content` is encoded; see enum-level docs for values.
        encoding: &'a str,
        /// The content, encoded per `encoding`.
        content: &'a str,
    },

    /// Unified diff produced by a `--diff` flag.
    ///
    /// Example: `{"reason":"diff","subcommand":"replace","content":"--- a/f\n..."}`
    Diff {
        /// Subcommand that produced this diff.
        subcommand: &'a str,
        /// UTF-8 diff text with LF line endings.
        content: &'a str,
    },

    /// Final message — always the last line emitted for one invocation.
    ///
    /// Example: `{"reason":"finished","success":true}`
    Finished {
        /// Whether the subcommand completed without error.
        success: bool,
    },
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn to_val(msg: &Msg<'_>) -> Value {
        serde_json::to_value(msg).expect("serialize")
    }

    // ── reason field ─────────────────────────────────────────────────────────

    #[test]
    fn status_reason_is_status() {
        let v = to_val(&Msg::Status {
            verb: "do",
            message: "m",
        });
        assert_eq!(v["reason"], "status");
    }

    #[test]
    fn warning_reason_is_warning() {
        let v = to_val(&Msg::Warning { message: "w" });
        assert_eq!(v["reason"], "warning");
    }

    #[test]
    fn error_reason_is_error() {
        let v = to_val(&Msg::Error { message: "e" });
        assert_eq!(v["reason"], "error");
    }

    #[test]
    fn data_reason_is_data() {
        let v = to_val(&Msg::Data {
            subcommand: "read",
            encoding: "text",
            content: "c",
        });
        assert_eq!(v["reason"], "data");
    }

    #[test]
    fn diff_reason_is_diff() {
        let v = to_val(&Msg::Diff {
            subcommand: "replace",
            content: "d",
        });
        assert_eq!(v["reason"], "diff");
    }

    #[test]
    fn finished_reason_is_finished() {
        let v = to_val(&Msg::Finished { success: true });
        assert_eq!(v["reason"], "finished");
    }

    // ── field names stable ────────────────────────────────────────────────────

    #[test]
    fn status_fields() {
        let v = to_val(&Msg::Status {
            verb: "replace",
            message: "file.txt: 3 replacements",
        });
        assert_eq!(v["verb"], "replace");
        assert_eq!(v["message"], "file.txt: 3 replacements");
        // No unexpected fields.
        assert_eq!(v.as_object().unwrap().len(), 3); // reason + verb + message
    }

    #[test]
    fn warning_fields() {
        let v = to_val(&Msg::Warning { message: "odd" });
        assert_eq!(v["message"], "odd");
        assert_eq!(v.as_object().unwrap().len(), 2); // reason + message
    }

    #[test]
    fn error_fields() {
        let v = to_val(&Msg::Error { message: "boom" });
        assert_eq!(v["message"], "boom");
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    #[test]
    fn data_fields() {
        let v = to_val(&Msg::Data {
            subcommand: "read",
            encoding: "text",
            content: "hello\n",
        });
        assert_eq!(v["subcommand"], "read");
        assert_eq!(v["encoding"], "text");
        assert_eq!(v["content"], "hello\n");
        assert_eq!(v.as_object().unwrap().len(), 4); // reason + 3 fields
    }

    #[test]
    fn diff_fields() {
        let v = to_val(&Msg::Diff {
            subcommand: "replace",
            content: "--- a\n",
        });
        assert_eq!(v["subcommand"], "replace");
        assert_eq!(v["content"], "--- a\n");
        assert_eq!(v.as_object().unwrap().len(), 3);
    }

    #[test]
    fn finished_success_true() {
        let v = to_val(&Msg::Finished { success: true });
        assert_eq!(v["success"], true);
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    #[test]
    fn finished_success_false() {
        let v = to_val(&Msg::Finished { success: false });
        assert_eq!(v["success"], false);
    }

    // ── NDJSON line format ────────────────────────────────────────────────────

    #[test]
    fn serialises_to_single_line() {
        let s = serde_json::to_string(&Msg::Status {
            verb: "v",
            message: "m",
        })
        .unwrap();
        // Must not contain literal newlines (content escapes are fine).
        assert!(!s.contains('\n'), "JSON must be a single line: {s:?}");
    }

    #[test]
    fn data_content_with_newlines_is_escaped() {
        let content = "line1\nline2\n";
        let s = serde_json::to_string(&Msg::Data {
            subcommand: "read",
            encoding: "text",
            content,
        })
        .unwrap();
        // serde_json escapes \n inside strings.
        assert!(
            s.contains(r"\n"),
            "newline should be escaped in JSON: {s:?}"
        );
        assert!(
            !s.contains('\n'),
            "no literal newline in NDJSON line: {s:?}"
        );
    }

    #[test]
    fn control_chars_in_message_are_escaped() {
        let v = to_val(&Msg::Error {
            message: "line1\r\nline2",
        });
        // The round-tripped value should equal the original string.
        assert_eq!(v["message"].as_str().unwrap(), "line1\r\nline2");
    }

    #[test]
    fn unicode_content_round_trips() {
        let content = "こんにちは\n";
        let s = serde_json::to_string(&Msg::Data {
            subcommand: "read",
            encoding: "text",
            content,
        })
        .unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["content"].as_str().unwrap(), content);
    }

    #[test]
    fn empty_content_serialises() {
        let v = to_val(&Msg::Data {
            subcommand: "read",
            encoding: "text",
            content: "",
        });
        assert_eq!(v["content"], "");
    }

    #[test]
    fn message_format_default_is_human() {
        assert_eq!(MessageFormat::default(), MessageFormat::Human);
    }
}
