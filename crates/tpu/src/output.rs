// Copyright (c) 2026, Michael Grier

//! `Output` trait and implementations for the tpu stdout pipeline.
//!
//! Subcommands deliver payloads through [`Output`] rather than writing to
//! `io::stdout()` directly.  This decouples payload encoding (plain text,
//! raw bytes, structured JSON) from the I/O target, making it trivial to
//! supply a `Vec<u8>` in tests.
//!
//! Two concrete implementations cover the two `--message-format` modes:
//!
//! - [`HumanOutput`] — writes payload verbatim to the underlying writer;
//!   JSON values are rendered via the mandatory `"rendered"` field.
//! - [`JsonOutput`] — wraps every payload in a newline-delimited JSON
//!   (`{"reason":"data",...}`) envelope.
//!
//! Use `tpu_emit!` for the common case of a plain-text emit without file
//! or range provenance.
//!
//! ## Write-failure policy
//!
//! All write failures panic.  In practice `tpu` is consumed by the MCP server
//! (full stdout read before exit) so broken-pipe (`EPIPE`) is not a real-world
//! scenario; OOM and programming errors are the only realistic failure causes,
//! both of which warrant a panic rather than a propagated `io::Error`.

use std::{
    fmt,
    io::{self, Write},
};

use crate::{data_format, escape};

// ── Output trait ──────────────────────────────────────────────────────────────

/// Interface through which subcommands deliver payloads to stdout.
///
/// The three methods correspond to the three output concerns:
///
/// - [`emit`][Output::emit] — plain text, expressed as `fmt::Arguments<'_>`
///   so the call site pays zero heap cost; implementations materialise to a
///   `String` only when necessary (e.g. JSON mode).
/// - [`emit_binary`][Output::emit_binary] — raw bytes.
/// - [`emit_json`][Output::emit_json] — a structured `serde_json::Value`.
///
/// `subcommand`, `file`, and `range` are provenance metadata.  `subcommand`
/// identifies the producing operation.  `file` and `range` are optional span
/// annotations (mirroring rustc diagnostics); JSON implementations embed them
/// in the NDJSON envelope when `Some`, human implementations ignore them
/// (content is self-describing).
///
/// **Callers never branch on format** — the implementation handles encoding.
pub trait Output {
    /// Emit a plain-text payload.
    ///
    /// `args` is produced by `format_args!()`.  Use `tpu_emit!` for the
    /// common case that omits file and range provenance.
    fn emit(
        &mut self,
        subcommand: &str,
        file: Option<&str>,
        range: Option<&str>,
        args: fmt::Arguments<'_>,
    );

    /// Emit a binary payload.
    ///
    /// [`HumanOutput`] writes the bytes as escaped text (via
    /// [`crate::escape::encode_bytes`], no trailing newline);
    /// [`JsonOutput`] encodes them as RFC 4648 Base64 inside a
    /// `{"reason":"data","encoding":"bytes-base64",...}` envelope.
    fn emit_binary(
        &mut self,
        subcommand: &str,
        file: Option<&str>,
        range: Option<&str>,
        bytes: &[u8],
    );

    /// Emit a binary payload with an optional `hashes` array in the JSON envelope.
    ///
    /// `hashes` is a pre-built `serde_json::Value::Array` containing hash
    /// entry objects (`{"algo","range","value"}`).  In human mode the default
    /// implementation silently delegates to [`emit_binary`][Output::emit_binary]
    /// so `.rsp` files that include `--hash` work transparently regardless of
    /// `--message-format`.  In JSON mode the `hashes` array is appended to the
    /// `bytes-base64` envelope.
    fn emit_binary_hashed(
        &mut self,
        subcommand: &str,
        file: Option<&str>,
        range: Option<&str>,
        bytes: &[u8],
        hashes: &serde_json::Value,
    ) {
        // Default: ignore hashes (human mode).
        let _ = hashes;
        self.emit_binary(subcommand, file, range, bytes);
    }

    /// Emit a binary payload followed by a line terminator.
    ///
    /// [`HumanOutput`] overrides this to write the escaped bytes and `\n`.
    /// [`JsonOutput`] uses the default because the NDJSON envelope already
    /// provides the line terminator.
    fn emit_binary_ln(
        &mut self,
        subcommand: &str,
        file: Option<&str>,
        range: Option<&str>,
        bytes: &[u8],
    ) {
        self.emit_binary(subcommand, file, range, bytes);
    }

    /// Emit a structured JSON payload.
    ///
    /// `value` must always contain a `"rendered"` string field — the
    /// human-readable form of the data.  [`HumanOutput`] extracts and writes
    /// `rendered` verbatim, ignoring all other fields.  [`JsonOutput`] emits
    /// `value` as a single NDJSON line unchanged.
    ///
    /// # Panics
    ///
    /// [`HumanOutput`] panics when `value["rendered"]` is absent or is not a
    /// string.  Callers must always supply the `"rendered"` field.
    fn emit_json(
        &mut self,
        subcommand: &str,
        file: Option<&str>,
        range: Option<&str>,
        value: &serde_json::Value,
    );
}

// ── tpu_emit! macro ───────────────────────────────────────────────────────────

/// Thin wrapper around [`Output::emit`] for call sites without file or range
/// provenance.
///
/// # Example
///
/// ```rust,ignore
/// tpu_emit!(out, "read", "{}", content);
/// // equivalent to:
/// out.emit("read", None, None, format_args!("{}", content));
/// ```
#[macro_export]
macro_rules! tpu_emit {
    ($out:expr, $sub:expr, $($arg:tt)*) => {
        $out.emit($sub, None, None, format_args!($($arg)*))
    };
}

// ── HumanOutput ───────────────────────────────────────────────────────────────

/// [`Output`] implementation that writes payload verbatim to a [`Write`] sink.
///
/// Designed for `--message-format=human` (the default): content goes directly
/// to the underlying writer without any JSON wrapping.  `file` and `range`
/// provenance fields are ignored — the content is self-describing.
pub struct HumanOutput {
    out: Box<dyn Write + Send>,
}

impl Output for HumanOutput {
    fn emit(
        &mut self,
        _subcommand: &str,
        _file: Option<&str>,
        _range: Option<&str>,
        args: fmt::Arguments<'_>,
    ) {
        self.out
            .write_fmt(args)
            .expect("HumanOutput::emit: write failed");
    }

    fn emit_binary(
        &mut self,
        _subcommand: &str,
        _file: Option<&str>,
        _range: Option<&str>,
        bytes: &[u8],
    ) {
        let escaped = escape::encode_bytes(bytes);
        self.out
            .write_all(escaped.as_bytes())
            .expect("HumanOutput::emit_binary: write failed");
    }

    fn emit_binary_ln(
        &mut self,
        _subcommand: &str,
        _file: Option<&str>,
        _range: Option<&str>,
        bytes: &[u8],
    ) {
        let escaped = escape::encode_bytes(bytes);
        self.out
            .write_all(escaped.as_bytes())
            .expect("HumanOutput::emit_binary_ln: write failed");
        self.out
            .write_all(b"\n")
            .expect("HumanOutput::emit_binary_ln: write failed");
    }

    fn emit_json(
        &mut self,
        _subcommand: &str,
        _file: Option<&str>,
        _range: Option<&str>,
        value: &serde_json::Value,
    ) {
        let rendered = value["rendered"].as_str().expect(
            "HumanOutput::emit_json: `rendered` field is absent or not a string \
             — callers must always supply it",
        );
        self.out
            .write_all(rendered.as_bytes())
            .expect("HumanOutput::emit_json: write failed");
    }
}

// ── JsonOutput ────────────────────────────────────────────────────────────────

/// [`Output`] implementation that emits NDJSON envelopes to a [`Write`] sink.
///
/// Designed for `--message-format=json`.  Each method wraps its payload in a
/// `{"reason":"data",...}` object followed by a newline.  `file` and `range`
/// provenance fields are included in the envelope when `Some`.
pub struct JsonOutput {
    out: Box<dyn Write + Send>,
}

impl JsonOutput {
    /// Serialise `value` as a single NDJSON line and write it to `out`.
    fn write_line(&mut self, value: &serde_json::Value) {
        let mut line = serde_json::to_string(value).expect("JsonOutput: JSON serialization failed");
        line.push('\n');
        self.out
            .write_all(line.as_bytes())
            .expect("JsonOutput: write failed");
    }
}

impl Output for JsonOutput {
    fn emit(
        &mut self,
        subcommand: &str,
        file: Option<&str>,
        range: Option<&str>,
        args: fmt::Arguments<'_>,
    ) {
        let content = fmt::format(args);
        let mut obj = serde_json::json!({
            "reason": "data",
            "subcommand": subcommand,
            "encoding": "text",
            "content": content,
        });
        if let Some(f) = file {
            obj["file"] = serde_json::Value::String(f.to_owned());
        }
        if let Some(r) = range {
            obj["range"] = serde_json::Value::String(r.to_owned());
        }
        self.write_line(&obj);
    }

    fn emit_binary(
        &mut self,
        subcommand: &str,
        file: Option<&str>,
        range: Option<&str>,
        bytes: &[u8],
    ) {
        let encoded = data_format::encode_base64(bytes);
        let mut obj = serde_json::json!({
            "reason": "data",
            "subcommand": subcommand,
            "encoding": "bytes-base64",
            "content": encoded,
        });
        if let Some(f) = file {
            obj["file"] = serde_json::Value::String(f.to_owned());
        }
        if let Some(r) = range {
            obj["range"] = serde_json::Value::String(r.to_owned());
        }
        self.write_line(&obj);
    }

    fn emit_binary_hashed(
        &mut self,
        subcommand: &str,
        file: Option<&str>,
        range: Option<&str>,
        bytes: &[u8],
        hashes: &serde_json::Value,
    ) {
        let encoded = data_format::encode_base64(bytes);
        let mut obj = serde_json::json!({
            "reason": "data",
            "subcommand": subcommand,
            "encoding": "bytes-base64",
            "content": encoded,
            "hashes": hashes,
        });
        if let Some(f) = file {
            obj["file"] = serde_json::Value::String(f.to_owned());
        }
        if let Some(r) = range {
            obj["range"] = serde_json::Value::String(r.to_owned());
        }
        self.write_line(&obj);
    }

    fn emit_json(
        &mut self,
        _subcommand: &str,
        _file: Option<&str>,
        _range: Option<&str>,
        value: &serde_json::Value,
    ) {
        self.write_line(value);
    }
}

// ── Constructors ──────────────────────────────────────────────────────────────

/// Returns a [`HumanOutput`] backed by real `io::stdout()`.
pub fn human_output() -> Box<dyn Output> {
    Box::new(HumanOutput {
        out: Box::new(io::stdout()),
    })
}

/// Returns a [`HumanOutput`] backed by an arbitrary [`Write`] sink.
///
/// Library callers (such as `tpu-mcp`) use this to capture output into a
/// `Vec<u8>` buffer rather than writing to stdout.
#[allow(dead_code)] // Used by tpu-mcp (library consumer), not by the tpu binary.
pub fn human_output_to(writer: Box<dyn Write + Send>) -> Box<dyn Output> {
    Box::new(HumanOutput { out: writer })
}

/// Returns a [`JsonOutput`] backed by real `io::stdout()`.
pub fn json_output() -> Box<dyn Output> {
    Box::new(JsonOutput {
        out: Box::new(io::stdout()),
    })
}

/// Returns a [`JsonOutput`] backed by an arbitrary [`Write`] sink.
///
/// Library callers (such as `tpu-mcp`) use this to capture NDJSON output into
/// a `Vec<u8>` buffer rather than writing to stdout.
#[allow(dead_code)] // Used by tpu-mcp (library consumer), not by the tpu binary.
pub fn json_output_to(writer: Box<dyn Write + Send>) -> Box<dyn Output> {
    Box::new(JsonOutput { out: writer })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::Value;

    use super::*;

    // ── Test infrastructure ───────────────────────────────────────────────────

    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // SAFETY: tests are single-threaded.
    unsafe impl Send for BufWriter {}

    fn capture_human() -> (HumanOutput, Arc<Mutex<Vec<u8>>>) {
        let data: Arc<Mutex<Vec<u8>>> = Default::default();
        let out = HumanOutput {
            out: Box::new(BufWriter(data.clone())),
        };
        (out, data)
    }

    fn capture_json() -> (JsonOutput, Arc<Mutex<Vec<u8>>>) {
        let data: Arc<Mutex<Vec<u8>>> = Default::default();
        let out = JsonOutput {
            out: Box::new(BufWriter(data.clone())),
        };
        (out, data)
    }

    fn read_str(data: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(data.lock().unwrap().clone()).expect("UTF-8")
    }

    fn parse_ndjson(data: &Arc<Mutex<Vec<u8>>>) -> Vec<Value> {
        read_str(data)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).expect("valid JSON line"))
            .collect()
    }

    // ── HumanOutput::emit ────────────────────────────────────────────────────

    #[test]
    fn human_emit_writes_text_verbatim() {
        let (mut out, buf) = capture_human();
        out.emit("read", None, None, format_args!("hello"));
        assert_eq!(read_str(&buf), "hello");
    }

    #[test]
    fn human_emit_formats_args() {
        let (mut out, buf) = capture_human();
        out.emit("read", None, None, format_args!("{} + {} = {}", 1, 2, 3));
        assert_eq!(read_str(&buf), "1 + 2 = 3");
    }

    #[test]
    fn human_emit_empty_args() {
        let (mut out, buf) = capture_human();
        out.emit("read", None, None, format_args!(""));
        assert_eq!(read_str(&buf), "");
    }

    #[test]
    fn human_emit_ignores_file_and_range() {
        let (mut out, buf) = capture_human();
        out.emit(
            "read",
            Some("config.txt"),
            Some("1-10"),
            format_args!("content"),
        );
        // file and range must not appear in the output
        let s = read_str(&buf);
        assert_eq!(s, "content");
        assert!(!s.contains("config.txt"));
        assert!(!s.contains("1-10"));
    }

    #[test]
    fn human_emit_multiple_calls_accumulate() {
        let (mut out, buf) = capture_human();
        out.emit("read", None, None, format_args!("line1\n"));
        out.emit("read", None, None, format_args!("line2\n"));
        assert_eq!(read_str(&buf), "line1\nline2\n");
    }

    // ── HumanOutput::emit_binary ─────────────────────────────────────────────

    #[test]
    fn human_emit_binary_writes_escaped_bytes() {
        let (mut out, buf) = capture_human();
        out.emit_binary("read", None, None, &[0x4D, 0x5A, 0x00, 0x00]);
        // M, Z, \0, \0 — printable ASCII passes through; NUL → \0 escape.
        assert_eq!(read_str(&buf), "MZ\\0\\0");
    }

    #[test]
    fn human_emit_binary_empty_slice_writes_nothing() {
        let (mut out, buf) = capture_human();
        out.emit_binary("read", None, None, &[]);
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn human_emit_binary_all_byte_values() {
        let (mut out, buf) = capture_human();
        let all: Vec<u8> = (0..=255).collect();
        out.emit_binary("read", None, None, &all);
        assert_eq!(read_str(&buf), crate::escape::encode_bytes(&all));
    }

    #[test]
    fn human_emit_binary_ignores_provenance() {
        let (mut out, buf) = capture_human();
        out.emit_binary("read", Some("foo.bin"), Some("0-512"), &[0xDE, 0xAD]);
        // 0xDE → \xde, 0xAD → \xad.
        assert_eq!(read_str(&buf), "\\xde\\xad");
    }

    #[test]
    fn human_emit_binary_ln_appends_newline() {
        let (mut out, buf) = capture_human();
        out.emit_binary_ln("readex", None, None, &[0x4D, 0x5A]);
        assert_eq!(read_str(&buf), "MZ\n");
    }

    #[test]
    fn human_emit_binary_ln_empty_is_just_newline() {
        let (mut out, buf) = capture_human();
        out.emit_binary_ln("readex", None, None, &[]);
        assert_eq!(read_str(&buf), "\n");
    }

    // ── HumanOutput::emit_json ───────────────────────────────────────────────

    #[test]
    fn human_emit_json_writes_rendered_field() {
        let (mut out, buf) = capture_human();
        let v = serde_json::json!({"reason": "data", "rendered": "hello world"});
        out.emit_json("read", None, None, &v);
        assert_eq!(read_str(&buf), "hello world");
    }

    #[test]
    fn human_emit_json_ignores_other_fields() {
        let (mut out, buf) = capture_human();
        let v = serde_json::json!({
            "reason": "data",
            "subcommand": "read",
            "encoding": "text",
            "content": "raw",
            "rendered": "pretty",
        });
        out.emit_json("read", None, None, &v);
        assert_eq!(read_str(&buf), "pretty");
    }

    #[test]
    fn human_emit_json_ignores_provenance_params() {
        let (mut out, buf) = capture_human();
        let v = serde_json::json!({"rendered": "data"});
        out.emit_json("read", Some("file.txt"), Some("5-10"), &v);
        let s = read_str(&buf);
        assert_eq!(s, "data");
        assert!(!s.contains("file.txt"));
    }

    #[test]
    fn human_emit_json_empty_rendered() {
        let (mut out, buf) = capture_human();
        let v = serde_json::json!({"rendered": ""});
        out.emit_json("read", None, None, &v);
        assert_eq!(read_str(&buf), "");
    }

    #[test]
    #[should_panic(expected = "`rendered` field is absent")]
    fn human_emit_json_panics_when_rendered_absent() {
        let (mut out, _buf) = capture_human();
        let v = serde_json::json!({"reason": "data"});
        out.emit_json("read", None, None, &v);
    }

    #[test]
    #[should_panic(expected = "`rendered` field is absent")]
    fn human_emit_json_panics_when_rendered_is_not_string() {
        let (mut out, _buf) = capture_human();
        let v = serde_json::json!({"rendered": 42});
        out.emit_json("read", None, None, &v);
    }

    // ── JsonOutput::emit ─────────────────────────────────────────────────────

    #[test]
    fn json_emit_reason_is_data() {
        let (mut out, buf) = capture_json();
        out.emit("read", None, None, format_args!("hello"));
        let lines = parse_ndjson(&buf);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["reason"], "data");
    }

    #[test]
    fn json_emit_encoding_is_text() {
        let (mut out, buf) = capture_json();
        out.emit("read", None, None, format_args!("hello"));
        let lines = parse_ndjson(&buf);
        assert_eq!(lines[0]["encoding"], "text");
    }

    #[test]
    fn json_emit_subcommand_field() {
        let (mut out, buf) = capture_json();
        out.emit("readex", None, None, format_args!("data"));
        let lines = parse_ndjson(&buf);
        assert_eq!(lines[0]["subcommand"], "readex");
    }

    #[test]
    fn json_emit_content_is_formatted_args() {
        let (mut out, buf) = capture_json();
        out.emit("read", None, None, format_args!("{} items", 42));
        let lines = parse_ndjson(&buf);
        assert_eq!(lines[0]["content"], "42 items");
    }

    #[test]
    fn json_emit_without_provenance_omits_file_and_range() {
        let (mut out, buf) = capture_json();
        out.emit("read", None, None, format_args!("data"));
        let lines = parse_ndjson(&buf);
        assert!(lines[0].get("file").is_none());
        assert!(lines[0].get("range").is_none());
    }

    #[test]
    fn json_emit_with_file_includes_file_field() {
        let (mut out, buf) = capture_json();
        out.emit("read", Some("config.txt"), None, format_args!("data"));
        let lines = parse_ndjson(&buf);
        assert_eq!(lines[0]["file"], "config.txt");
        assert!(lines[0].get("range").is_none());
    }

    #[test]
    fn json_emit_with_range_includes_range_field() {
        let (mut out, buf) = capture_json();
        out.emit("read", None, Some("10-20"), format_args!("data"));
        let lines = parse_ndjson(&buf);
        assert_eq!(lines[0]["range"], "10-20");
        assert!(lines[0].get("file").is_none());
    }

    #[test]
    fn json_emit_with_file_and_range() {
        let (mut out, buf) = capture_json();
        out.emit(
            "read",
            Some("config.txt"),
            Some("10-20"),
            format_args!("content"),
        );
        let lines = parse_ndjson(&buf);
        assert_eq!(lines[0]["file"], "config.txt");
        assert_eq!(lines[0]["range"], "10-20");
        assert_eq!(lines[0]["content"], "content");
    }

    #[test]
    fn json_emit_empty_content() {
        let (mut out, buf) = capture_json();
        out.emit("read", None, None, format_args!(""));
        let lines = parse_ndjson(&buf);
        assert_eq!(lines[0]["content"], "");
    }

    #[test]
    fn json_emit_each_call_is_one_ndjson_line() {
        let (mut out, buf) = capture_json();
        out.emit("read", None, None, format_args!("first"));
        out.emit("read", None, None, format_args!("second"));
        let lines = parse_ndjson(&buf);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["content"], "first");
        assert_eq!(lines[1]["content"], "second");
    }

    // ── JsonOutput::emit_binary ──────────────────────────────────────────────

    #[test]
    fn json_emit_binary_encoding_is_bytes_base64() {
        let (mut out, buf) = capture_json();
        out.emit_binary("read", None, None, &[0x4D, 0x5A]);
        let lines = parse_ndjson(&buf);
        assert_eq!(lines[0]["encoding"], "bytes-base64");
    }

    #[test]
    fn json_emit_binary_content_is_base64() {
        let (mut out, buf) = capture_json();
        // 0x4D 0x5A → base64 "TV0="... let's compute: 0x4D=77, 0x5A=90
        // binary: 01001101 01011010 (2 bytes)
        // groups of 6: 010011 010101 1010xx
        // 19=T, 21=V, 26=a, padding=
        // Actually: encode_base64(&[0x4D, 0x5A]) = "TV0=" ? No...
        // Let me compute: [0x4D, 0x5A, pad]
        // combined = (0x4D << 16) | (0x5A << 8) | 0 = 0x4D5A00
        // bits: 0100 1101 0101 1010 0000 0000
        // group 18: 010011 = 19 = T
        // group 12: 010101 = 21 = V
        // group 6: 101000 = 40 = o (since chunk.len()==2, use alphabet[40])
        // group 0: not used for chunk.len()==2, = '='
        // so: "TVo=" -- let's just verify the round-trip
        let data = &[0x4D, 0x5A];
        let expected = data_format::encode_base64(data);
        out.emit_binary("read", None, None, data);
        let lines = parse_ndjson(&buf);
        assert_eq!(lines[0]["content"].as_str().unwrap(), expected);
    }

    #[test]
    fn json_emit_binary_empty_bytes() {
        let (mut out, buf) = capture_json();
        out.emit_binary("read", None, None, &[]);
        let lines = parse_ndjson(&buf);
        // base64 of empty is empty string
        assert_eq!(lines[0]["content"], "");
        assert_eq!(lines[0]["encoding"], "bytes-base64");
    }

    #[test]
    fn json_emit_binary_with_file_and_range() {
        let (mut out, buf) = capture_json();
        out.emit_binary("read", Some("foo.bin"), Some("0-512"), &[0xFF]);
        let lines = parse_ndjson(&buf);
        assert_eq!(lines[0]["file"], "foo.bin");
        assert_eq!(lines[0]["range"], "0-512");
        assert_eq!(lines[0]["reason"], "data");
    }

    #[test]
    fn json_emit_binary_without_provenance_omits_fields() {
        let (mut out, buf) = capture_json();
        out.emit_binary("read", None, None, &[0x00]);
        let lines = parse_ndjson(&buf);
        assert!(lines[0].get("file").is_none());
        assert!(lines[0].get("range").is_none());
    }

    #[test]
    fn json_emit_binary_round_trips_via_decode() {
        let (mut out, buf) = capture_json();
        let original = b"Hello, world!";
        out.emit_binary("write", None, None, original);
        let lines = parse_ndjson(&buf);
        let encoded = lines[0]["content"].as_str().unwrap();
        let decoded = data_format::decode(&data_format::DataFormat::Base64, encoded).unwrap();
        assert_eq!(decoded, original);
    }

    // ── JsonOutput::emit_json ────────────────────────────────────────────────

    #[test]
    fn json_emit_json_passes_value_through_unchanged() {
        let (mut out, buf) = capture_json();
        let v = serde_json::json!({
            "reason": "data",
            "subcommand": "read",
            "encoding": "text",
            "content": "hello\n",
            "rendered": "hello\n",
        });
        out.emit_json("read", None, None, &v);
        let lines = parse_ndjson(&buf);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["reason"], "data");
        assert_eq!(lines[0]["content"], "hello\n");
        assert_eq!(lines[0]["rendered"], "hello\n");
    }

    #[test]
    fn json_emit_json_is_one_ndjson_line() {
        let (mut out, buf) = capture_json();
        let v = serde_json::json!({"reason": "finished", "success": true});
        out.emit_json("read", None, None, &v);
        let raw = read_str(&buf);
        // Exactly one non-empty line
        let non_empty: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(non_empty.len(), 1);
        let parsed: Value = serde_json::from_str(non_empty[0]).unwrap();
        assert_eq!(parsed["success"], true);
    }

    #[test]
    fn json_emit_json_ignores_provenance_params() {
        // file and range params are ignored — caller is responsible for
        // including them in the value when needed.
        let (mut out, buf) = capture_json();
        let v = serde_json::json!({"reason": "data", "rendered": "x"});
        out.emit_json("read", Some("ignored.txt"), Some("0-1"), &v);
        let lines = parse_ndjson(&buf);
        // The value passed through should not have been modified
        assert_eq!(lines[0], v);
    }

    // ── tpu_emit! macro ──────────────────────────────────────────────────────

    #[test]
    fn tpu_emit_macro_human_mode() {
        let (mut out, buf) = capture_human();
        tpu_emit!(out, "read", "{} lines", 5);
        assert_eq!(read_str(&buf), "5 lines");
    }

    #[test]
    fn tpu_emit_macro_json_mode() {
        let (mut out, buf) = capture_json();
        tpu_emit!(out, "read", "hello");
        let lines = parse_ndjson(&buf);
        assert_eq!(lines[0]["content"], "hello");
        assert!(lines[0].get("file").is_none());
        assert!(lines[0].get("range").is_none());
    }

    #[test]
    fn tpu_emit_macro_passes_no_provenance() {
        // Confirmed above: calling tpu_emit! always passes None for file/range
        let (mut out, buf) = capture_json();
        tpu_emit!(out, "readex", "data {}", 42);
        let lines = parse_ndjson(&buf);
        assert_eq!(lines[0]["subcommand"], "readex");
        assert_eq!(lines[0]["content"], "data 42");
        assert!(lines[0].get("file").is_none());
    }
}
