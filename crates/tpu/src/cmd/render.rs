// Copyright (c) 2026, Michael Grier

//! `tpu render` — populate a file by Mustache-style token replacement.
//!
//! The template source is one of:
//!
//! - `--template <STRING>` — inline template on the command line;
//! - `--template-file <PATH>` — template loaded from a file (encoding is
//!   detected and decoded the same way `tpu read` would decode it);
//! - stdin — when neither flag is supplied.
//!
//! Tokens are written as `{{NAME}}` and substituted with values supplied via
//! repeatable `--var KEY=VALUE` pairs. Whitespace inside the braces is
//! tolerated, so `{{ NAME }}` and `{{NAME}}` both expand to the value of
//! `NAME`. To emit a literal `{{` in the output, escape it as `\{{`.
//!
//! The rendered text is written to the output file via [`crate::cmd::write::run`]
//! so the resulting file inherits the standard write-time mojibake guard,
//! atomic .bak handling, and encoding-preservation behaviour.

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::Arc,
};

use harrier::{encoding::SourceConfig, source::Source};

use crate::{
    encoding::{BomPolicy, OutputEncoding},
    mojibake::WritePolicy,
    IoMode,
};

/// Behaviour when the template references a token that is not in `vars`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MissingPolicy {
    /// Fail the render with an error listing all missing token names. Default.
    #[default]
    Error,
    /// Substitute the empty string for missing tokens.
    Empty,
    /// Leave the literal `{{NAME}}` placeholder in place.
    Leave,
}

/// Rendered document plus a list of missing token names (in the order they
/// were first encountered).
#[derive(Debug, Default)]
pub struct RenderResult {
    /// Substitution count (counts every substitution, including repeated uses
    /// of the same token).
    pub substitutions: usize,
    /// Token names referenced in the template but absent from `vars` (in the
    /// order of first appearance, deduplicated).
    pub missing: Vec<String>,
    /// Count of distinct token names that were actually referenced by the
    /// template (both found and missing, deduplicated). Useful for composing
    /// human-readable summaries.
    pub referenced: usize,
}

/// Substitute `{{KEY}}` tokens in `template` using `vars` and return the
/// rendered string.
///
/// Returns an error only when [`MissingPolicy::Error`] is selected and the
/// template references at least one token that is not in `vars`.
pub fn render_str(
    template: &str,
    vars: &BTreeMap<String, String>,
    missing: MissingPolicy,
) -> Result<(String, RenderResult), Box<dyn std::error::Error>> {
    let mut out = String::with_capacity(template.len());
    let mut report = RenderResult::default();
    let mut seen_tokens = std::collections::BTreeSet::<String>::new();
    let bytes = template.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Escape sequence: \{{ → emit literal "{{" and skip both characters.
        if bytes[i] == b'\\' && i + 2 < bytes.len() && bytes[i + 1] == b'{' && bytes[i + 2] == b'{'
        {
            out.push_str("{{");
            i += 3;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find the matching `}}`.
            let close = find_close(bytes, i + 2);
            match close {
                Some(end) => {
                    let key = std::str::from_utf8(&bytes[i + 2..end])
                        .map_err(|e| format!("render: token is not valid UTF-8: {e}"))?
                        .trim();
                    if key.is_empty() {
                        return Err("render: empty `{{}}` placeholder is not allowed".into());
                    }
                    match vars.get(key) {
                        Some(v) => {
                            out.push_str(v);
                            report.substitutions += 1;
                            seen_tokens.insert(key.to_string());
                        }
                        None => {
                            if !report.missing.iter().any(|k| k == key) {
                                report.missing.push(key.to_string());
                                seen_tokens.insert(key.to_string());
                            }
                            match missing {
                                MissingPolicy::Error => {
                                    // Defer error until we have walked the
                                    // full template so the diagnostic can
                                    // list every missing key at once.
                                }
                                MissingPolicy::Empty => {}
                                MissingPolicy::Leave => {
                                    // Preserve the original slice (including any
                                    // whitespace inside the braces) rather than
                                    // reconstructing from the trimmed key name.
                                    out.push_str(&template[i..end + 2]);
                                }
                            }
                        }
                    }
                    i = end + 2;
                    continue;
                }
                None => {
                    return Err(
                        "render: unterminated `{{` placeholder (no matching `}}`)".into()
                    );
                }
            }
        }
        // Advance by one Unicode scalar value (not one byte) so non-ASCII
        // UTF-8 sequences in the literal text are emitted intact.
        let ch = template[i..]
            .chars()
            .next()
            .ok_or("render: internal error: expected UTF-8 character boundary")?;
        out.push(ch);
        i += ch.len_utf8();
    }
    report.referenced = seen_tokens.len();
    if matches!(missing, MissingPolicy::Error) && !report.missing.is_empty() {
        return Err(format!(
            "render: template references undefined token{}: {}",
            if report.missing.len() == 1 { "" } else { "s" },
            report.missing.join(", ")
        )
        .into());
    }
    Ok((out, report))
}

fn find_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j + 1 < bytes.len() {
        if bytes[j] == b'}' && bytes[j + 1] == b'}' {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Read a template file and return its decoded UTF-8/LF text.
///
/// Mirrors the encoding behaviour of `tpu read` (any encoding harrier can
/// detect; LF-normalised view).
pub fn load_template_file(
    file: &Path,
    io_mode: IoMode,
) -> Result<String, Box<dyn std::error::Error>> {
    let branch = crate::open_as_branch(file, io_mode)?;
    let len = branch.byte_len();
    let source = Source::new(Arc::clone(&branch), SourceConfig::default())?;
    let bom_len = source.bom_len();
    let encoding = source.encoding();
    let lines_iter = source.as_lines()?;
    let view = lines_iter.view_range(bom_len as u64..len)?;
    let (cow, _) = encoding.decode_without_bom_handling(&view.bytes);
    Ok(cow.into_owned())
}

/// Parse a `KEY=VALUE` argument. The value may contain `=` characters.
pub fn parse_var(arg: &str) -> Result<(String, String), String> {
    let (k, v) = arg
        .split_once('=')
        .ok_or_else(|| format!("--var {arg:?}: expected KEY=VALUE form"))?;
    let k = k.trim();
    if k.is_empty() {
        return Err(format!("--var {arg:?}: KEY is empty"));
    }
    if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(format!(
            "--var {arg:?}: KEY may only contain ASCII letters, digits, '_' or '-'"
        ));
    }
    Ok((k.to_string(), v.to_string()))
}

/// Run the `render` subcommand: load template, substitute, write the output.
#[allow(clippy::too_many_arguments)]
pub fn run(
    output: &Path,
    template_inline: Option<&str>,
    template_file: Option<&Path>,
    template_stdin: Option<&str>,
    vars: &BTreeMap<String, String>,
    missing: MissingPolicy,
    io_mode: IoMode,
    policy: WritePolicy,
) -> Result<RenderResult, Box<dyn std::error::Error>> {
    let template_owned: String;
    let template: &str = match (template_inline, template_file, template_stdin) {
        (Some(s), None, None) => s,
        (None, Some(p), None) => {
            template_owned = load_template_file(p, io_mode)?;
            &template_owned
        }
        (None, None, Some(s)) => s,
        (None, None, None) => {
            return Err("render: must supply --template, --template-file, or pipe a template on stdin".into());
        }
        _ => return Err("render: --template, --template-file, and stdin are mutually exclusive".into()),
    };

    let (rendered, report) = render_str(template, vars, missing)?;

    // If the destination does not exist yet, create the parent directory so
    // template-driven scaffolds can write into fresh trees.
    if !output.exists() {
        if let Some(parent) = output.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent).map_err(|e| {
                    format!("render: cannot create parent {}: {e}", parent.display())
                })?;
            }
        }
    }

    crate::cmd::write::run(
        output,
        &rendered,
        OutputEncoding::Preserve,
        BomPolicy::default(),
        None,
        None,
        io_mode,
        policy,
    )?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
    }

    #[test]
    fn simple_substitution() {
        let v = vars(&[("NAME", "world")]);
        let (out, r) = render_str("hello {{NAME}}!", &v, MissingPolicy::Error).unwrap();
        assert_eq!(out, "hello world!");
        assert_eq!(r.substitutions, 1);
    }

    #[test]
    fn whitespace_in_braces_is_tolerated() {
        let v = vars(&[("X", "1")]);
        let (out, _) = render_str("{{ X }}-{{X}}", &v, MissingPolicy::Error).unwrap();
        assert_eq!(out, "1-1");
    }

    #[test]
    fn missing_token_errors_by_default() {
        let v = vars(&[]);
        let err = render_str("{{X}}", &v, MissingPolicy::Error).unwrap_err();
        assert!(err.to_string().contains("undefined"));
    }

    #[test]
    fn missing_token_empty_substitutes_empty() {
        let v = vars(&[]);
        let (out, _) = render_str("[{{X}}]", &v, MissingPolicy::Empty).unwrap();
        assert_eq!(out, "[]");
    }

    #[test]
    fn missing_token_leave_keeps_placeholder() {
        let v = vars(&[]);
        let (out, _) = render_str("[{{X}}]", &v, MissingPolicy::Leave).unwrap();
        assert_eq!(out, "[{{X}}]");
    }

    #[test]
    fn escape_keeps_literal_braces() {
        let v = vars(&[("X", "1")]);
        let (out, _) = render_str("\\{{X}}", &v, MissingPolicy::Error).unwrap();
        assert_eq!(out, "{{X}}");
    }

    #[test]
    fn unterminated_brace_is_error() {
        let v = vars(&[]);
        assert!(render_str("oops {{X", &v, MissingPolicy::Empty).is_err());
    }

    #[test]
    fn parse_var_basic() {
        assert_eq!(parse_var("FOO=bar").unwrap(), ("FOO".into(), "bar".into()));
        assert_eq!(parse_var("X=1=2").unwrap(), ("X".into(), "1=2".into()));
        assert!(parse_var("FOO").is_err());
        assert!(parse_var("=bar").is_err());
        assert!(parse_var("BAD KEY=v").is_err());
    }
}
