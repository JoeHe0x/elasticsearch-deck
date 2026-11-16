//! Request block splitting and request-line parsing (Kibana Dev Tools style: `METHOD path[?query]` followed by a body).

use std::ops::Range;

pub(crate) const METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "HEAD"];

/// A request block. All ranges are byte offsets into the input text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub method: String,
    /// Path without the query string; may be empty or partial while the user is typing.
    pub path: String,
    pub query: Option<String>,
    pub line: Range<usize>,
    /// Text between this request line and the next request line (or end of input).
    pub body: Range<usize>,
}

/// Parses one request line. The method is case-insensitive and normalized to uppercase;
/// a bare method (`GET`) also counts, with an empty path, so it can be completed.
fn parse_line(line: &str) -> Option<(String, String, Option<String>)> {
    let line = line.trim();
    let (m, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let method = METHODS.iter().find(|x| x.eq_ignore_ascii_case(m))?;
    let rest = rest.trim();
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q.to_string())),
        None => (rest, None),
    };
    Some((method.to_string(), path.to_string(), query))
}

/// Splits the whole editor text into request blocks. Comments and blank lines before a request line are ignored.
pub fn split_blocks(text: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let end = offset + raw.len();
        if let Some((method, path, query)) = parse_line(raw) {
            if let Some(prev) = blocks.last_mut() {
                prev.body.end = offset;
            }
            blocks.push(Block {
                method,
                path,
                query,
                line: offset..end,
                body: end..end,
            });
        } else if let Some(cur) = blocks.last_mut() {
            cur.body.end = end;
        }
        offset = end;
    }
    blocks
}

/// The block containing the cursor (byte offset): from its request line up to the next request line.
pub fn block_at(text: &str, cursor: usize) -> Option<Block> {
    split_blocks(text)
        .into_iter()
        .find(|b| cursor >= b.line.start && cursor <= b.body.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_table() {
        let cases = [
            ("GET /idx/_search", Some(("GET", "/idx/_search", None))),
            (
                "post _bulk?refresh=true",
                Some(("POST", "_bulk", Some("refresh=true"))),
            ),
            ("  DELETE /idx  ", Some(("DELETE", "/idx", None))),
            ("GET", Some(("GET", "", None))),
            ("GET /a?", Some(("GET", "/a", Some("")))),
            ("{\"query\": {}}", None),
            ("GETX /a", None),
            ("# GET /a", None),
            ("", None),
        ];
        for (input, want) in cases {
            let want = want.map(|(m, p, q)| (m.to_string(), p.to_string(), q.map(String::from)));
            assert_eq!(parse_line(input), want, "input: {input:?}");
        }
    }

    #[test]
    fn splits_blocks_with_bodies() {
        let text = "# comment\nGET /a/_search\n{\n  \"size\": 1\n}\n\nPOST /b/_doc\n{}\n";
        let bs = split_blocks(text);
        assert_eq!(bs.len(), 2);
        assert_eq!(&text[bs[0].line.clone()], "GET /a/_search\n");
        assert_eq!(&text[bs[0].body.clone()], "{\n  \"size\": 1\n}\n\n");
        assert_eq!(bs[1].method, "POST");
        assert_eq!(&text[bs[1].body.clone()], "{}\n");
    }

    #[test]
    fn crlf_and_no_trailing_newline() {
        let text = "GET /a\r\n{}\r\nGET /b";
        let bs = split_blocks(text);
        assert_eq!(bs.len(), 2);
        assert_eq!(bs[0].path, "/a");
        assert_eq!(bs[1].path, "/b");
        assert_eq!(bs[1].body, text.len()..text.len());
    }

    #[test]
    fn block_at_cursor() {
        let text = "GET /a\n{}\nGET /b\n{}\n";
        assert_eq!(block_at(text, 0).unwrap().path, "/a");
        assert_eq!(block_at(text, 8).unwrap().path, "/a");
        assert_eq!(block_at(text, 12).unwrap().path, "/b");
        assert!(block_at("# only a comment\n", 3).is_none());
    }
}
