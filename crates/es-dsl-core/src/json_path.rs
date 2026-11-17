//! Tolerant JSON prefix scanner: given the text before the cursor, computes the JSON path and key/value slot.
//!
//! It keeps only a nesting stack instead of building a tree, so any prefix (unclosed brackets or strings, missing colons) is safe.

/// A path segment: object key or array index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

/// What kind of position the cursor is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    /// Key position; `prefix` is the partial key typed so far (without quotes).
    Key { prefix: String },
    /// Value position (object value, array element or top level) outside a string.
    Value,
    /// Inside an open string in a value position; `prefix` is the text typed so far.
    Str { prefix: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub path: Vec<Seg>,
    pub slot: Slot,
}

enum Frame {
    Obj { key: Option<String>, in_key: bool },
    Arr { idx: usize },
}

/// A completed object key found in a document: the path of the object holding it, its name and its
/// byte range (quotes included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyTok {
    pub parent: Vec<Seg>,
    pub name: String,
    pub range: std::ops::Range<usize>,
}

/// Every object key in `text`, in order. Tolerates the same broken input as [`cursor_at`].
pub fn keys(text: &str) -> Vec<KeyTok> {
    let mut out = Vec::new();
    scan(text, &mut |t| out.push(t));
    out
}

/// String values assigned to `key`, with the path of the object that holds the key. Used to read
/// scope switches such as `nested.path`; a value still being typed (no closing quote yet) is skipped.
pub(crate) fn string_values(text: &str, key: &str) -> Vec<(Vec<Seg>, String)> {
    keys(text)
        .into_iter()
        .filter(|k| k.name == key)
        .filter_map(|k| {
            let rest = text[k.range.end..].trim_start();
            let rest = rest.strip_prefix(':')?.trim_start();
            let mut chars = rest.strip_prefix('"')?.chars();
            let mut value = String::new();
            loop {
                match chars.next()? {
                    '\\' => value.push(chars.next()?),
                    '"' => return Some((k.parent, value)),
                    c => value.push(c),
                }
            }
        })
        .collect()
}

/// The JSON path at `cursor` (byte offset) inside the request block that holds it; empty when the
/// cursor is on a request line or outside any block.
pub fn path_at(text: &str, cursor: usize) -> Vec<Seg> {
    let Some(block) = crate::block_at(text, cursor) else {
        return Vec::new();
    };
    let start = block.body.start.min(cursor);
    cursor_at(&text[start..cursor]).path
}

/// Scans `prefix` (all text before the cursor) and returns the path and slot at the cursor.
pub fn cursor_at(prefix: &str) -> Cursor {
    let (stack, open_str) = scan(prefix, &mut |_| {});
    let mut path = Vec::new();
    let mut slot = Slot::Value;
    let last = stack.len().saturating_sub(1);
    for (i, f) in stack.iter().enumerate() {
        match f {
            Frame::Obj { key, in_key } if i == last && *in_key => {
                slot = Slot::Key {
                    prefix: open_str
                        .clone()
                        .unwrap_or_else(|| bare_word(prefix).to_string()),
                };
            }
            Frame::Obj { key: Some(k), .. } => {
                path.push(Seg::Key(k.clone()));
                if i == last
                    && let Some(p) = &open_str
                {
                    slot = Slot::Str { prefix: p.clone() };
                }
            }
            Frame::Obj { key: None, .. } => {}
            Frame::Arr { idx } => {
                path.push(Seg::Index(*idx));
                if i == last
                    && let Some(p) = &open_str
                {
                    slot = Slot::Str { prefix: p.clone() };
                }
            }
        }
    }
    Cursor { path, slot }
}

/// The unquoted word right before the cursor: a key typed without its opening quote.
fn bare_word(text: &str) -> &str {
    let start = text
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || "_.-".contains(*c))
        .last()
        .map_or(text.len(), |(i, _)| i);
    &text[start..]
}

/// Walks `text`, reporting each completed key, and returns the open nesting stack plus any unterminated string.
fn scan(text: &str, on_key: &mut dyn FnMut(KeyTok)) -> (Vec<Frame>, Option<String>) {
    let mut stack: Vec<Frame> = Vec::new();
    let mut chars = text.char_indices();
    // Some(buf) means we are inside a string; `start` is where it opened
    let mut open_str: Option<String> = None;
    let mut start = 0;

    while let Some((at, c)) = chars.next() {
        if let Some(buf) = open_str.as_mut() {
            match c {
                '\\' => {
                    // Escape: swallow the next char, which is enough for path decisions
                    if let Some((_, n)) = chars.next() {
                        buf.push(n);
                    }
                }
                '"' => {
                    let s = open_str.take().unwrap_or_default();
                    if matches!(stack.last(), Some(Frame::Obj { in_key: true, .. })) {
                        on_key(KeyTok {
                            parent: path_of(&stack),
                            name: s.clone(),
                            range: start..at + 1,
                        });
                    }
                    if let Some(Frame::Obj { key, in_key }) = stack.last_mut()
                        && *in_key
                    {
                        *key = Some(s);
                        *in_key = false;
                    }
                }
                _ => buf.push(c),
            }
            continue;
        }
        match c {
            '"' => {
                open_str = Some(String::new());
                start = at;
            }
            '{' => stack.push(Frame::Obj {
                key: None,
                in_key: true,
            }),
            '[' => stack.push(Frame::Arr { idx: 0 }),
            '}' | ']' => {
                stack.pop();
            }
            ',' => match stack.last_mut() {
                Some(Frame::Obj { key, in_key }) => {
                    *key = None;
                    *in_key = true;
                }
                Some(Frame::Arr { idx }) => *idx += 1,
                None => {}
            },
            _ => {}
        }
    }
    (stack, open_str)
}

/// Path of the containers on `stack`; a key still being typed in the innermost object is not part of it.
fn path_of(stack: &[Frame]) -> Vec<Seg> {
    stack
        .iter()
        .filter_map(|f| match f {
            Frame::Obj { key: Some(k), .. } => Some(Seg::Key(k.clone())),
            Frame::Obj { key: None, .. } => None,
            Frame::Arr { idx } => Some(Seg::Index(*idx)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn path_at_reads_the_block_body_only() {
        let text = "GET a\n{}\n\nGET b/_search\n{\"query\": {\"bool\": {\"must\": [{\"te";
        assert!(path_at(text, 3).is_empty());
        assert_eq!(
            path_at(text, text.len()),
            [
                Seg::Key("query".into()),
                Seg::Key("bool".into()),
                Seg::Key("must".into()),
                Seg::Index(0)
            ]
        );
    }

    use super::*;

    fn k(s: &str) -> Seg {
        Seg::Key(s.into())
    }
    fn key_slot(p: &str) -> Slot {
        Slot::Key { prefix: p.into() }
    }

    #[test]
    fn table() {
        let cases: Vec<(&str, Vec<Seg>, Slot)> = vec![
            ("", vec![], Slot::Value),
            ("{term", vec![], key_slot("term")),
            ("{\"a\": 1, qu", vec![], key_slot("qu")),
            ("{\"a\": tr", vec![k("a")], Slot::Value),
            ("{", vec![], key_slot("")),
            ("{\"qu", vec![], key_slot("qu")),
            ("{\"query\":", vec![k("query")], Slot::Value),
            ("{\"query\": {", vec![k("query")], key_slot("")),
            ("{\"query\": {\"bo", vec![k("query")], key_slot("bo")),
            (
                "{\"query\":{\"bool\":{\"must\":[{\"ma",
                vec![k("query"), k("bool"), k("must"), Seg::Index(0)],
                key_slot("ma"),
            ),
            (
                "{\"query\":{\"bool\":{\"must\":[{},{",
                vec![k("query"), k("bool"), k("must"), Seg::Index(1)],
                key_slot(""),
            ),
            ("{\"a\":1,", vec![], key_slot("")),
            ("{\"a\":{\"b\":1},\"c\":", vec![k("c")], Slot::Value),
            // Brackets and escaped quotes inside strings do not affect structure
            ("{\"a\":\"x{[\\\"\",", vec![], key_slot("")),
            // Missing colon, unterminated string
            ("{\"a\"", vec![k("a")], Slot::Value),
            (
                "{\"a\":\"abc",
                vec![k("a")],
                Slot::Str {
                    prefix: "abc".into(),
                },
            ),
            (
                "{\"a\":[\"x",
                vec![k("a"), Seg::Index(0)],
                Slot::Str { prefix: "x".into() },
            ),
            // Extra closing brackets must not panic
            ("}}]", vec![], Slot::Value),
        ];
        for (input, path, slot) in cases {
            let c = cursor_at(input);
            assert_eq!((c.path, c.slot), (path, slot), "input: {input:?}");
        }
    }

    #[test]
    fn lists_keys_with_their_parents() {
        let text = "{\"a\":{\"b\":1},\"c\":[{\"d\":2}],\"e";
        let got: Vec<_> = keys(text)
            .into_iter()
            .map(|k| (k.parent, k.name, &text[k.range]))
            .collect();
        assert_eq!(
            got,
            vec![
                (vec![], "a".to_string(), "\"a\""),
                (vec![k("a")], "b".to_string(), "\"b\""),
                (vec![], "c".to_string(), "\"c\""),
                (vec![k("c"), Seg::Index(0)], "d".to_string(), "\"d\""),
            ]
        );
    }

    #[test]
    fn string_values_carry_the_object_path() {
        let text = r#"{"query":{"nested":{"path":"reviews","query":{"nested":{"path":"items"}}}}"#;
        assert_eq!(
            string_values(text, "path"),
            [
                (vec![k("query"), k("nested")], "reviews".to_string()),
                (
                    vec![k("query"), k("nested"), k("query"), k("nested")],
                    "items".to_string()
                ),
            ]
        );
        // A value still being typed has no closing quote and is not reported.
        assert!(string_values(r#"{"nested":{"path":"rev"#, "path").is_empty());
        assert!(string_values(r#"{"path": 3}"#, "path").is_empty());
    }

    #[test]
    fn no_panic_on_any_prefix() {
        let doc = "{\"query\":{\"bool\":{\"must\":[{\"match\":{\"f\":\"a\\\"b\"}}]}},\"size\":10}";
        for (i, _) in doc.char_indices().chain([(doc.len(), ' ')]) {
            cursor_at(&doc[..i]);
        }
    }
}
