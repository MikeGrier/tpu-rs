// Copyright (c) 2026, Michael Grier

/// If `argv` consists of exactly the program name plus a single argument
/// starting with `@`, return the response-file path indicated by the
/// remainder of that argument.
///
/// Returns `None` when the pattern does not match (argv length ≠ 2, or the
/// second element does not begin with `@`).
pub fn try_rsp_path(argv: &[String]) -> Option<&str> {
    if argv.len() == 2 {
        argv[1].strip_prefix('@')
    } else {
        None
    }
}

/// Tokenise the text content of a response file.
///
/// Tokens are delimited by ASCII whitespace (space U+0020, tab U+0009,
/// carriage-return U+000D, line-feed U+000A).  Runs of whitespace between
/// tokens are collapsed; leading and trailing whitespace is ignored.
///
/// A `"` character begins a *quoted token*.  Inside a quoted token:
/// - `\\` decodes to a single `\`.
/// - `\"` decodes to a single `"`.
/// - Any other `\X` passes both `\` and `X` through unchanged.
/// - Spaces and other ASCII whitespace are literal (not token separators).
/// - The opening `"` must be matched by a closing `"` before end of input;
///   an unmatched opening quote is an error.
///
/// Quoted and unquoted content may not be mixed mid-token in this
/// implementation; `"` always starts a fresh quoted token.
pub fn tokenize_rsp(content: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut tokens: Vec<String> = Vec::new();
    let mut chars = content.chars().peekable();

    loop {
        // Skip ASCII whitespace between tokens.
        while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            chars.next();
        }

        match chars.next() {
            None => break,

            Some('"') => {
                // Quoted token — spaces are literal; \\ and \" are the only
                // recognised escape sequences.
                let mut token = String::new();
                loop {
                    match chars.next() {
                        None => {
                            return Err("response file: unmatched opening quote".into());
                        }
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('\\') => token.push('\\'),
                            Some('"') => token.push('"'),
                            Some(c) => {
                                // Unrecognised escape — pass both characters
                                // through unchanged.
                                token.push('\\');
                                token.push(c);
                            }
                            None => {
                                return Err("response file: unmatched opening quote".into());
                            }
                        },
                        Some(c) => token.push(c),
                    }
                }
                tokens.push(token);
            }

            Some(c) => {
                // Unquoted token — runs until the next ASCII whitespace.
                let mut token = String::new();
                token.push(c);
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_whitespace() {
                        break;
                    }
                    chars.next();
                    token.push(next);
                }
                tokens.push(token);
            }
        }
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::{tokenize_rsp, try_rsp_path};

    // ── try_rsp_path ─────────────────────────────────────────────────────────

    fn sv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rsp_path_detected_when_argv_len_2_with_at_prefix() {
        let argv = sv(&["tpu", "@args.rsp"]);
        assert_eq!(try_rsp_path(&argv), Some("args.rsp"));
    }

    #[test]
    fn rsp_path_detected_with_absolute_path() {
        let argv = sv(&["tpu", "@C:\\path\\to\\file.rsp"]);
        assert_eq!(try_rsp_path(&argv), Some("C:\\path\\to\\file.rsp"));
    }

    #[test]
    fn rsp_path_none_when_argv_len_1() {
        let argv = sv(&["tpu"]);
        assert_eq!(try_rsp_path(&argv), None);
    }

    #[test]
    fn rsp_path_none_when_argv_len_3() {
        let argv = sv(&["tpu", "@a.rsp", "extra"]);
        assert_eq!(try_rsp_path(&argv), None);
    }

    #[test]
    fn rsp_path_none_when_second_arg_has_no_at() {
        let argv = sv(&["tpu", "read"]);
        assert_eq!(try_rsp_path(&argv), None);
    }

    #[test]
    fn rsp_path_bare_at_yields_empty_path() {
        // A bare "@" with nothing after it yields an empty path string.
        let argv = sv(&["tpu", "@"]);
        assert_eq!(try_rsp_path(&argv), Some(""));
    }

    // ── tokenize_rsp — simple unquoted tokens ────────────────────────────────

    #[test]
    fn tokenize_empty_string_gives_no_tokens() {
        assert_eq!(tokenize_rsp("").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn tokenize_single_token() {
        assert_eq!(tokenize_rsp("read").unwrap(), ["read"]);
    }

    #[test]
    fn tokenize_multiple_tokens_space_separated() {
        assert_eq!(
            tokenize_rsp("read --binary file.txt").unwrap(),
            ["read", "--binary", "file.txt"]
        );
    }

    #[test]
    fn tokenize_leading_whitespace_ignored() {
        assert_eq!(tokenize_rsp("   read").unwrap(), ["read"]);
    }

    #[test]
    fn tokenize_trailing_whitespace_ignored() {
        assert_eq!(tokenize_rsp("read   ").unwrap(), ["read"]);
    }

    #[test]
    fn tokenize_mixed_spaces_tabs_between_tokens() {
        assert_eq!(
            tokenize_rsp("read\t--binary\t file.txt").unwrap(),
            ["read", "--binary", "file.txt"]
        );
    }

    #[test]
    fn tokenize_newline_separated_tokens() {
        assert_eq!(
            tokenize_rsp("read\n--binary\nfile.txt").unwrap(),
            ["read", "--binary", "file.txt"]
        );
    }

    #[test]
    fn tokenize_crlf_separated_tokens() {
        assert_eq!(
            tokenize_rsp("read\r\n--binary\r\nfile.txt").unwrap(),
            ["read", "--binary", "file.txt"]
        );
    }

    #[test]
    fn tokenize_only_whitespace_gives_no_tokens() {
        assert_eq!(tokenize_rsp("  \t\n  ").unwrap(), Vec::<String>::new());
    }

    // ── tokenize_rsp — quoted tokens ──────────────────────────────────────────

    #[test]
    fn tokenize_quoted_token_with_spaces() {
        assert_eq!(tokenize_rsp("\"hello world\"").unwrap(), ["hello world"]);
    }

    #[test]
    fn tokenize_quoted_token_with_tabs() {
        assert_eq!(tokenize_rsp("\"hello\tworld\"").unwrap(), ["hello\tworld"]);
    }

    #[test]
    fn tokenize_quoted_token_empty_string() {
        assert_eq!(tokenize_rsp("\"\"").unwrap(), [""]);
    }

    #[test]
    fn tokenize_backslash_backslash_in_quoted_token() {
        assert_eq!(tokenize_rsp("\"\\\\\"").unwrap(), ["\\"]);
    }

    #[test]
    fn tokenize_backslash_quote_in_quoted_token() {
        assert_eq!(tokenize_rsp("\"\\\"\"").unwrap(), ["\""]);
    }

    #[test]
    fn tokenize_unrecognised_escape_passes_through() {
        // \n inside quotes is not a recognised escape; both \ and n are kept.
        assert_eq!(tokenize_rsp("\"\\n\"").unwrap(), ["\\n"]);
    }

    #[test]
    fn tokenize_mixed_unquoted_and_quoted_tokens() {
        assert_eq!(
            tokenize_rsp("read \"my file.txt\"").unwrap(),
            ["read", "my file.txt"]
        );
    }

    #[test]
    fn tokenize_multiple_quoted_tokens() {
        assert_eq!(tokenize_rsp("\"a b\" \"c d\"").unwrap(), ["a b", "c d"]);
    }

    #[test]
    fn tokenize_quoted_token_at_start_then_unquoted() {
        assert_eq!(
            tokenize_rsp("\"first token\" second").unwrap(),
            ["first token", "second"]
        );
    }

    // ── tokenize_rsp — error cases ────────────────────────────────────────────

    #[test]
    fn tokenize_unmatched_opening_quote_is_error() {
        assert!(tokenize_rsp("\"unclosed").is_err());
    }

    #[test]
    fn tokenize_unmatched_quote_after_tokens_is_error() {
        assert!(tokenize_rsp("read \"unclosed").is_err());
    }

    #[test]
    fn tokenize_bare_at_eof_after_backslash_in_quoted_is_error() {
        // Opening quote followed by a backslash then EOF.
        assert!(tokenize_rsp("\"\\").is_err());
    }

    #[test]
    fn tokenize_error_message_mentions_unmatched_quote() {
        let e = tokenize_rsp("\"unterminated").unwrap_err();
        assert!(e.to_string().contains("unmatched"), "error: {e}");
    }
}
