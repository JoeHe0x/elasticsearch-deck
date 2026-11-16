//! Re-indents the JSON body of a request block.

use std::ops::Range;

use serde_json::Value;

use crate::block_at;

/// A replacement for `range` of the original text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
}

/// Pretty-prints the body of the request at `cursor`. `None` when there is nothing safe to do: no
/// body, comment lines (they would be lost), several JSON values (NDJSON) or invalid JSON.
/// shortcut: numbers go through `serde_json`, so an integer beyond u64 or an exotic float spelling is
/// rewritten in its f64 form; switch on `arbitrary_precision` if that ever matters.
pub fn format_body(text: &str, cursor: usize) -> Option<Edit> {
    let block = block_at(text, cursor)?;
    let body = &text[block.body.clone()];
    if body
        .lines()
        .any(|l| l.trim_start().starts_with('#') || l.trim_start().starts_with("//"))
    {
        return None;
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    let pretty = serde_json::to_string_pretty(&value).ok()?;
    let start = block.body.start + (body.len() - body.trim_start().len());
    Some(Edit {
        range: start..start + trimmed.len(),
        text: pretty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, cursor: usize) -> Option<String> {
        let e = format_body(text, cursor)?;
        Some(format!(
            "{}{}{}",
            &text[..e.range.start],
            e.text,
            &text[e.range.end..]
        ))
    }

    #[test]
    fn formats_only_the_block_under_the_cursor_and_keeps_key_order() {
        let text = "GET a/_search\n{\"z\":1,\"a\":{\"b\":[1,2]}}\n\nGET b/_search\n{\"x\":1}\n";
        let out = apply(text, 5).unwrap();
        assert!(out.starts_with("GET a/_search\n{\n  \"z\": 1,\n  \"a\": {\n    \"b\": [\n"));
        assert!(out.ends_with("GET b/_search\n{\"x\":1}\n"));
    }

    #[test]
    fn leaves_unsafe_bodies_alone() {
        assert!(apply("GET a\n", 0).is_none());
        assert!(apply("GET a\n{\"a\":", 0).is_none());
        assert!(apply("POST _bulk\n{\"index\":{}}\n{\"a\":1}\n", 0).is_none());
        assert!(apply("GET a\n# keep me\n{\"a\":1}\n", 0).is_none());
    }
}
