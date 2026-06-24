// Copyright (c) 2026, Michael Grier

//! Shared types for output encoding normalisation.
//!
//! These types are used by subcommands that support `--utf8` and `--bom`
//! options.  The default behaviour (when neither flag is supplied) is to
//! preserve the source file's encoding and BOM exactly as found.

use std::str::FromStr;

/// Re-export so consumers can name the line-ending type as
/// `tpu::encoding::LineEnding` (the canonical type used throughout `tpu`'s
/// public API for `--line-ending` overrides and git-EOL normalisation).
#[allow(unused_imports)]
// Re-exported for tpu-mcp (library consumer); unused inside the tpu binary.
pub use harrier::encoding::LineEnding;

/// Whether a subcommand should re-encode output as UTF-8.
///
/// The default is [`OutputEncoding::Preserve`]: the file's native encoding is
/// kept.  [`OutputEncoding::Utf8`] forces UTF-8 output; the companion
/// [`BomPolicy`] then governs whether a BOM byte sequence is included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputEncoding {
    /// Keep the source file's encoding unchanged (default).
    #[default]
    Preserve,
    /// Re-encode output as UTF-8.
    Utf8,
}

/// How to handle the UTF-8 byte-order mark when `--utf8` is active.
///
/// Has no effect when [`OutputEncoding::Preserve`] is in use.
///
/// # Changing this default is a breaking change
///
/// The default is `Strip` (no BOM).  Future changes to the default must be
/// documented in `DESIGN-NOTES.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BomPolicy {
    /// Do not write a BOM in the output (default).
    #[default]
    Strip,
    /// Write a BOM only if the source file contained one.
    Preserve,
    /// Always write a BOM regardless of the source.
    Force,
}

impl FromStr for BomPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "strip" => Ok(BomPolicy::Strip),
            "preserve" => Ok(BomPolicy::Preserve),
            "force" => Ok(BomPolicy::Force),
            other => Err(format!(
                "invalid --bom value {other:?}: expected strip, preserve, or force"
            )),
        }
    }
}

/// Parse a line-ending name (`"lf"`, `"crlf"`, `"cr"`) into a
/// [`harrier::encoding::LineEnding`] value.
///
/// This is the canonical parse function shared by `tpu` CLI and `tpu-mcp`.
/// Callers pass the raw string from a `--line-ending` flag or an MCP JSON
/// field.  Returns `Err` with a human-readable message on unrecognised input.
#[allow(dead_code)] // Used by tpu-mcp (library consumer), not by the tpu binary.
pub fn parse_line_ending(
    s: &str,
) -> Result<harrier::encoding::LineEnding, Box<dyn std::error::Error>> {
    match s {
        "lf" => Ok(harrier::encoding::LineEnding::Lf),
        "crlf" => Ok(harrier::encoding::LineEnding::CrLf),
        "cr" => Ok(harrier::encoding::LineEnding::Cr),
        other => Err(
            format!("unrecognised line-ending value {other:?}; expected lf, crlf, or cr").into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bom_policy_from_str_strip() {
        assert_eq!("strip".parse::<BomPolicy>().unwrap(), BomPolicy::Strip);
    }

    #[test]
    fn bom_policy_from_str_preserve() {
        assert_eq!(
            "preserve".parse::<BomPolicy>().unwrap(),
            BomPolicy::Preserve
        );
    }

    #[test]
    fn bom_policy_from_str_force() {
        assert_eq!("force".parse::<BomPolicy>().unwrap(), BomPolicy::Force);
    }

    #[test]
    fn bom_policy_from_str_invalid() {
        let err = "auto".parse::<BomPolicy>().unwrap_err();
        assert!(err.contains("invalid --bom value"));
        assert!(err.contains("\"auto\""));
    }

    #[test]
    fn output_encoding_default_is_preserve() {
        assert_eq!(OutputEncoding::default(), OutputEncoding::Preserve);
    }

    #[test]
    fn bom_policy_default_is_strip() {
        assert_eq!(BomPolicy::default(), BomPolicy::Strip);
    }

    #[test]
    fn bom_policy_debug_roundtrip_strip() {
        assert!(format!("{:?}", BomPolicy::Strip).contains("Strip"));
    }

    #[test]
    fn bom_policy_debug_roundtrip_preserve() {
        assert!(format!("{:?}", BomPolicy::Preserve).contains("Preserve"));
    }

    #[test]
    fn bom_policy_debug_roundtrip_force() {
        assert!(format!("{:?}", BomPolicy::Force).contains("Force"));
    }

    #[test]
    fn bom_policy_from_str_empty_string_is_error() {
        assert!("".parse::<BomPolicy>().is_err());
    }

    #[test]
    fn bom_policy_from_str_case_sensitive() {
        // Values must be lowercase; "Strip" (titlecase) is not accepted.
        assert!("Strip".parse::<BomPolicy>().is_err());
        assert!("STRIP".parse::<BomPolicy>().is_err());
    }

    #[test]
    fn output_encoding_variants_are_distinct() {
        assert_ne!(OutputEncoding::Preserve, OutputEncoding::Utf8);
    }
}
