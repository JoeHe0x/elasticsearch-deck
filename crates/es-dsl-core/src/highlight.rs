//! Token classes for coloring a console buffer. The buffer is not plain JSON (each block opens with
//! a request line), so editors cannot reuse a JSON grammar; this classifier covers both parts.

use std::ops::Range;

use crate::request::{METHODS, split_blocks};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Method,
    /// An index, alias or id in the path.
    Target,
    /// An API segment of the path, such as `_search`.
    Endpoint,
    Query,
    Key,
    /// An object key the schema declares (`query`, `match_all`); only schema-aware callers know.
    Property,
    /// An object key the user chose (field or aggregation name); only schema-aware callers know.
    Field,
    String,
    Number,
    Keyword,
}

/// Ordered, non-overlapping spans (byte offsets). Anything left out is plain text.
pub fn tokens(text: &str) -> Vec<(Range<usize>, Token)> {
    let mut out = Vec::new();
    for block in split_blocks(text) {
        request_line(text, block.line.clone(), &mut out);
        body(text, block.body, &mut out);
    }
    out
}

/// Spans of a buffer that is plain JSON, such as a response body.
pub fn json_tokens(text: &str) -> Vec<(Range<usize>, Token)> {
    let mut out = Vec::new();
    body(text, 0..text.len(), &mut out);
    out
}

/// Byte ranges of each request, from its request line to its last non-blank character.
pub fn block_spans(text: &str) -> Vec<Range<usize>> {
    split_blocks(text)
        .into_iter()
        .map(|b| b.line.start..b.line.start + text[b.line.start..b.body.end].trim_end().len())
        .collect()
}

fn request_line(text: &str, line: Range<usize>, out: &mut Vec<(Range<usize>, Token)>) {
    let s = &text[line.clone()];
    let lead = s.len() - s.trim_start().len();
    let method_len = s[lead..]
        .find(char::is_whitespace)
        .unwrap_or(s.len() - lead);
    let start = line.start + lead;
    debug_assert!(
        METHODS
            .iter()
            .any(|m| m.eq_ignore_ascii_case(&s[lead..lead + method_len]))
    );
    out.push((start..start + method_len, Token::Method));
    let rest = &s[lead + method_len..];
    let path_at = lead + method_len + (rest.len() - rest.trim_start().len());
    let end = line.start + s.trim_end().len();
    let path_start = line.start + path_at;
    if path_start >= end {
        return;
    }
    let split = text[path_start..end]
        .find('?')
        .map_or(end, |q| path_start + q);
    let mut at = path_start;
    for seg in text[path_start..split].split('/') {
        if !seg.is_empty() {
            let token = if seg.starts_with('_') {
                Token::Endpoint
            } else {
                Token::Target
            };
            out.push((at..at + seg.len(), token));
        }
        at += seg.len() + 1;
    }
    if split < end {
        out.push((split..end, Token::Query));
    }
}

fn body(text: &str, range: Range<usize>, out: &mut Vec<(Range<usize>, Token)>) {
    let bytes = text.as_bytes();
    let mut i = range.start;
    while i < range.end {
        match bytes[i] {
            b'"' => {
                let start = i;
                i += 1;
                while i < range.end && bytes[i] != b'"' && bytes[i] != b'\n' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                // Cap at the range, and at the line when the string never closes.
                let closed = i < range.end && bytes[i] == b'"';
                i = (i + usize::from(closed)).min(range.end);
                let after = text[i..range.end].trim_start_matches([' ', '\t']);
                let key = closed && after.starts_with(':');
                out.push((start..i, if key { Token::Key } else { Token::String }));
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                while i < range.end
                    && matches!(bytes[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                {
                    i += 1;
                }
                out.push((start..i, Token::Number));
            }
            b if b.is_ascii_alphabetic() => {
                let start = i;
                while i < range.end && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                if matches!(&text[start..i], "true" | "false" | "null") {
                    out.push((start..i, Token::Keyword));
                }
            }
            _ => i += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(text: &str) -> Vec<(&str, Token)> {
        tokens(text)
            .into_iter()
            .map(|(r, t)| (&text[r], t))
            .collect()
    }

    #[test]
    fn colors_the_request_line_and_the_body() {
        let text = "GET idx/_search?size=1\n{\"q\": [1, -2.5e3, true], \"s\": \"x\\\"y\"}\n";
        assert_eq!(
            classes(text),
            [
                ("GET", Token::Method),
                ("idx", Token::Target),
                ("_search", Token::Endpoint),
                ("?size=1", Token::Query),
                ("\"q\"", Token::Key),
                ("1", Token::Number),
                ("-2.5e3", Token::Number),
                ("true", Token::Keyword),
                ("\"s\"", Token::Key),
                ("\"x\\\"y\"", Token::String),
            ]
        );
    }

    #[test]
    fn survives_partial_input() {
        for text in [
            "GET",
            "get  ",
            "POST x\n{\"",
            "POST x\n{\"a",
            "POST x\n{\"a\\",
            "PUT x\n\"\\",
            "PUT x\n\"\\é\"",
            "PUT x\n{\"é",
        ] {
            let spans = tokens(text);
            assert!(
                spans
                    .iter()
                    .all(|(r, _)| r.end <= text.len() && r.start <= r.end)
            );
        }
        assert_eq!(
            classes("POST x\n{\"a"),
            [
                ("POST", Token::Method),
                ("x", Token::Target),
                ("\"a", Token::String)
            ]
        );
    }

    #[test]
    fn a_block_ends_at_its_last_character() {
        let text = "GET a\n{}\n\nGET b\n";
        assert_eq!(block_spans(text), [0..8, 10..15]);
    }
}
