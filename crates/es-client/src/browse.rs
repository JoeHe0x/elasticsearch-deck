//! Requests behind the Data Browser: one page of an index or alias, as a plain `_search`.

use std::ops::Range;

use es_dsl_core::{Field, FieldKind};
use serde_json::{Value, json};

use crate::Request;

/// `index.max_result_window` when a cluster does not say otherwise.
pub const DEFAULT_WINDOW: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Asc,
    Desc,
}

/// What the browser shows: one page of `target`, filtered and sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// An index or alias name.
    pub target: String,
    pub from: u64,
    pub size: u64,
    /// A KQL expression (see [`kql`]), or Lucene syntax sent as a `query_string`; empty means every document.
    pub filter: String,
    /// A sortable field (see [`sort_field`]) and its direction.
    pub sort: Option<(String, Order)>,
}

impl Page {
    /// The request for this page. The filter only ever lands in the body, never in the path.
    pub fn request(&self) -> Request {
        let mut body = json!({ "from": self.from, "size": self.size, "track_total_hits": true });
        if !self.filter.trim().is_empty() {
            // Not KQL (or not the part of it understood): let Lucene syntax have a go.
            body["query"] = kql(&self.filter)
                .unwrap_or_else(|| json!({ "query_string": { "query": self.filter } }));
        }
        if let Some((field, order)) = &self.sort {
            let order = if *order == Order::Asc { "asc" } else { "desc" };
            body["sort"] = json!([{ field: order }]);
        }
        Request {
            method: "POST".into(),
            path: format!("{}/_search", encode(&self.target)),
            body: Some(body.to_string()),
        }
    }

    /// Whether the page after this one would still be inside `window` and the hits.
    pub fn has_next(&self, total: Option<u64>, window: u64) -> bool {
        let next = self.from + self.size;
        next + self.size <= window && total.is_none_or(|t| next < t)
    }
}

/// Translates the KQL subset the filter box offers into a query: `field : value`, `field : "a phrase"`,
/// `field : wild*`, `field : *` (exists), `field > 1` (also `>=`, `<`, `<=`), a bare value, and
/// `and` / `or` / `not` with parentheses. `None` when the text is not in that subset, so the caller
/// can fall back to Lucene syntax. Multi-word unquoted values (`message: why do you`) are not
/// supported; quote them.
pub fn kql(input: &str) -> Option<Value> {
    let tokens = kql_tokens(input);
    let mut at = 0;
    let query = kql_or(&tokens, &mut at)?;
    (at == tokens.len()).then_some(query)
}

#[derive(Debug, PartialEq)]
enum Tok {
    Open,
    Close,
    /// `:` or a comparison operator.
    Op(String),
    Quoted(String),
    Word(String),
}

fn kql_tokens(input: &str) -> Vec<Tok> {
    lex(input).into_iter().map(|(tok, _)| tok).collect()
}

/// Tokens with their byte ranges in `input`.
fn lex(input: &str) -> Vec<(Tok, Range<usize>)> {
    let mut out = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        let mut end = start + c.len_utf8();
        let tok = match c {
            c if c.is_whitespace() => {
                chars.next();
                continue;
            }
            '(' | ')' => {
                chars.next();
                if c == '(' { Tok::Open } else { Tok::Close }
            }
            ':' | '<' | '>' => {
                chars.next();
                let mut op = c.to_string();
                if c != ':' && chars.peek().map(|&(_, c)| c) == Some('=') {
                    chars.next();
                    op.push('=');
                    end += 1;
                }
                Tok::Op(op)
            }
            '"' => {
                chars.next();
                let mut text = String::new();
                for (i, c) in chars.by_ref() {
                    end = i + c.len_utf8();
                    if c == '"' {
                        break;
                    }
                    text.push(c);
                }
                Tok::Quoted(text)
            }
            _ => {
                let mut word = String::new();
                while let Some(&(i, c)) = chars.peek() {
                    if c.is_whitespace() || "():<>\"".contains(c) {
                        break;
                    }
                    word.push(c);
                    end = i + c.len_utf8();
                    chars.next();
                }
                Tok::Word(word)
            }
        };
        out.push((tok, start..end));
    }
    out
}

/// What a stretch of a filter is, for coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    Field,
    /// `and` / `or` / `not`, and the `:` and comparison operators.
    Operator,
    Number,
    Value,
}

/// Byte ranges of a filter's fields, operators and values. Lenient like [`kql`]: half-typed input
/// is colored as far as it goes.
pub fn kql_spans(input: &str) -> Vec<(Range<usize>, Span)> {
    let toks = lex(input);
    let mut out = Vec::new();
    for (i, (tok, range)) in toks.iter().enumerate() {
        let after_op = i > 0 && matches!(toks[i - 1].0, Tok::Op(_));
        let before_op = matches!(toks.get(i + 1), Some((Tok::Op(_), _)));
        let span = match tok {
            Tok::Open | Tok::Close => continue,
            Tok::Op(_) => Span::Operator,
            Tok::Quoted(_) => Span::Value,
            Tok::Word(w) if after_op => {
                if w.parse::<f64>().is_ok() {
                    Span::Number
                } else {
                    Span::Value
                }
            }
            Tok::Word(_) if before_op => Span::Field,
            Tok::Word(w)
                if ["and", "or", "not"]
                    .iter()
                    .any(|k| w.eq_ignore_ascii_case(k)) =>
            {
                Span::Operator
            }
            Tok::Word(_) => Span::Value,
        };
        out.push((range.clone(), span));
    }
    out
}

fn is_keyword(tok: Option<&Tok>, keyword: &str) -> bool {
    matches!(tok, Some(Tok::Word(w)) if w.eq_ignore_ascii_case(keyword))
}

/// `or` binds looser than `and`, as in Kibana.
fn kql_or(t: &[Tok], at: &mut usize) -> Option<Value> {
    let mut parts = vec![kql_and(t, at)?];
    while is_keyword(t.get(*at), "or") {
        *at += 1;
        parts.push(kql_and(t, at)?);
    }
    Some(kql_bool("should", parts))
}

fn kql_and(t: &[Tok], at: &mut usize) -> Option<Value> {
    let mut parts = vec![kql_not(t, at)?];
    while is_keyword(t.get(*at), "and") {
        *at += 1;
        parts.push(kql_not(t, at)?);
    }
    Some(kql_bool("must", parts))
}

fn kql_bool(clause: &str, mut parts: Vec<Value>) -> Value {
    if parts.len() == 1 {
        return parts.remove(0);
    }
    let mut body = json!({ "bool": { clause: parts } });
    if clause == "should" {
        body["bool"]["minimum_should_match"] = json!(1);
    }
    body
}

fn kql_not(t: &[Tok], at: &mut usize) -> Option<Value> {
    if is_keyword(t.get(*at), "not") {
        *at += 1;
        return Some(json!({ "bool": { "must_not": [kql_not(t, at)?] } }));
    }
    if t.get(*at) == Some(&Tok::Open) {
        *at += 1;
        let inner = kql_or(t, at)?;
        if t.get(*at) != Some(&Tok::Close) {
            return None;
        }
        *at += 1;
        return Some(inner);
    }
    kql_term(t, at)
}

fn kql_term(t: &[Tok], at: &mut usize) -> Option<Value> {
    let first = t.get(*at)?;
    *at += 1;
    let Tok::Op(op) = t.get(*at).unwrap_or(&Tok::Close) else {
        // A bare value is looked up in every field.
        return match first {
            Tok::Word(w) if !["and", "or", "not"].contains(&w.to_lowercase().as_str()) => {
                Some(json!({ "query_string": { "query": w } }))
            }
            Tok::Quoted(q) => Some(json!({ "query_string": { "query": format!("\"{q}\"") } })),
            _ => None,
        };
    };
    let Tok::Word(field) = first else { return None };
    *at += 1;
    let (value, quoted) = match t.get(*at)? {
        Tok::Word(w) => (w.as_str(), false),
        Tok::Quoted(q) => (q.as_str(), true),
        _ => return None,
    };
    *at += 1;
    Some(match (op.as_str(), quoted) {
        (":", false) if value == "*" => json!({ "exists": { "field": field } }),
        (":", false) if value.contains('*') => {
            json!({ "query_string": { "query": value, "default_field": field } })
        }
        (":", true) => json!({ "match_phrase": { field: value } }),
        (":", false) => json!({ "match": { field: value } }),
        (">", _) => json!({ "range": { field: { "gt": value } } }),
        (">=", _) => json!({ "range": { field: { "gte": value } } }),
        ("<", _) => json!({ "range": { field: { "lt": value } } }),
        _ => json!({ "range": { field: { "lte": value } } }),
    })
}

/// The request that reads the result window of `target` (an alias may span several indices).
pub fn window_request(target: &str) -> Request {
    let setting = "settings.index.max_result_window";
    let default = "defaults.index.max_result_window";
    Request {
        method: "GET".into(),
        path: format!(
            "{}/_settings?include_defaults=true&filter_path=*.{setting},*.{default}",
            encode(target)
        ),
        body: None,
    }
}

/// The smallest window among the indices in a [`window_request`] answer; [`DEFAULT_WINDOW`] when
/// the answer has none, so a surprising body never blocks paging.
pub fn parse_window(body: &str) -> u64 {
    let Ok(Value::Object(indices)) = serde_json::from_str::<Value>(body) else {
        return DEFAULT_WINDOW;
    };
    let read = |index: &Value, group: &str| {
        let v = index.get(group)?.get("index")?.get("max_result_window")?;
        v.as_u64().or_else(|| v.as_str()?.parse().ok())
    };
    indices
        .values()
        .filter_map(|i| read(i, "settings").or_else(|| read(i, "defaults")))
        .min()
        .unwrap_or(DEFAULT_WINDOW)
}

/// The reason an error body gives, or `None` when it is not one.
pub fn error_reason(body: &str) -> Option<String> {
    let json: Value = serde_json::from_str(body).ok()?;
    let error = json.get("error")?;
    let cause = error
        .get("root_cause")
        .and_then(|c| c.get(0))
        .unwrap_or(error);
    let reason = cause.get("reason").or_else(|| error.get("reason"))?;
    Some(reason.as_str()?.to_string())
}

/// The field to sort by when the column `name` is clicked: the column itself when the mapping lets
/// it sort, its `.keyword` sub-field for analyzed text, otherwise `None` (a sort on any other
/// field makes Elasticsearch reject the request).
pub fn sort_field(name: &str, fields: &[Field]) -> Option<String> {
    let of = |n: &str| fields.iter().find(|f| f.name == n && !f.nested);
    match of(name)?.kind() {
        FieldKind::Keyword
        | FieldKind::Numeric
        | FieldKind::Date
        | FieldKind::Boolean
        | FieldKind::Ip
        | FieldKind::Version => Some(name.to_string()),
        FieldKind::Text => {
            let sub = format!("{name}.keyword");
            of(&sub).map(|_| sub)
        }
        _ => None,
    }
}

/// Percent-encodes what a path segment cannot carry. Index names are restricted, so this only
/// matters for odd names; wildcards and commas are encoded too, which keeps a target one name.
fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(filter: &str, sort: Option<(&str, Order)>) -> Page {
        Page {
            target: "logs".into(),
            from: 100,
            size: 50,
            filter: filter.into(),
            sort: sort.map(|(f, o)| (f.into(), o)),
        }
    }

    fn body(p: &Page) -> Value {
        serde_json::from_str(&p.request().body.unwrap()).unwrap()
    }

    #[test]
    fn the_request_carries_paging_and_only_what_is_set() {
        let plain = body(&page("", None));
        assert_eq!(
            plain,
            json!({ "from": 100, "size": 50, "track_total_hits": true })
        );
        let full = body(&page("a:1 AND b > 2", Some(("price", Order::Desc))));
        assert_eq!(
            full["query"],
            json!({ "bool": { "must": [
                { "match": { "a": "1" } },
                { "range": { "b": { "gt": "2" } } }
            ] } })
        );
        let lucene = body(&page("a:[1 TO 2]", None));
        assert_eq!(
            lucene["query"],
            json!({ "query_string": { "query": "a:[1 TO 2]" } })
        );
        assert_eq!(full["sort"], json!([{ "price": "desc" }]));
        assert_eq!(body(&page("   ", None)).get("query"), None);
    }

    #[test]
    fn kql_translates_the_documented_forms() {
        let cases = [
            (
                "Title : S*",
                json!({ "query_string": { "query": "S*", "default_field": "Title" } }),
            ),
            ("m: *", json!({ "exists": { "field": "m" } })),
            (
                "m: \"why do you\"",
                json!({ "match_phrase": { "m": "why do you" } }),
            ),
            (
                "d < \"2021-01-02\"",
                json!({ "range": { "d": { "lt": "2021-01-02" } } }),
            ),
            ("a >= 100", json!({ "range": { "a": { "gte": "100" } } })),
            ("hello", json!({ "query_string": { "query": "hello" } })),
            (
                "a:1 or b:2 and not (c:3)",
                json!({ "bool": { "minimum_should_match": 1, "should": [
                    { "match": { "a": "1" } },
                    { "bool": { "must": [
                        { "match": { "b": "2" } },
                        { "bool": { "must_not": [ { "match": { "c": "3" } } ] } }
                    ] } }
                ] } }),
            ),
        ];
        for (text, want) in cases {
            assert_eq!(kql(text), Some(want), "{text}");
        }
        for bad in ["a:", "a:1 and", "(a:1", "a:[1 TO 2]", "m: why do you", ":"] {
            assert_eq!(kql(bad), None, "{bad}");
        }
    }

    #[test]
    fn spans_mark_fields_operators_and_values() {
        let text = "Title : \"a b\" and rating >= 7.5";
        let got: Vec<_> = kql_spans(text)
            .into_iter()
            .map(|(r, s)| (&text[r], s))
            .collect();
        assert_eq!(
            got,
            [
                ("Title", Span::Field),
                (":", Span::Operator),
                ("\"a b\"", Span::Value),
                ("and", Span::Operator),
                ("rating", Span::Field),
                (">=", Span::Operator),
                ("7.5", Span::Number),
            ]
        );
    }

    #[test]
    fn the_filter_never_reaches_the_path() {
        let r = page("../_all/_delete?x", None).request();
        assert_eq!(
            (r.method.as_str(), r.path.as_str()),
            ("POST", "logs/_search")
        );
        let odd = Page {
            target: "a/b,c*".into(),
            ..page("", None)
        }
        .request();
        assert_eq!(odd.path, "a%2Fb%2Cc%2A/_search");
    }

    #[test]
    fn the_next_page_stays_inside_the_window_and_the_hits() {
        let p = page("", None); // rows 100..150
        assert!(p.has_next(Some(500), 10_000));
        assert!(!p.has_next(Some(150), 10_000), "no hits after row 150");
        assert!(p.has_next(None, 10_000));
        assert!(p.has_next(None, 200), "from 150, size 50 ends at 200");
        assert!(!p.has_next(None, 199), "that page would pass the window");
    }

    #[test]
    fn the_window_is_the_smallest_setting_or_the_default() {
        let one = r#"{"a":{"defaults":{"index":{"max_result_window":"10000"}}}}"#;
        assert_eq!(parse_window(one), 10_000);
        let two = r#"{"a":{"settings":{"index":{"max_result_window":"50000"}}},
                      "b":{"defaults":{"index":{"max_result_window":"10000"}}}}"#;
        assert_eq!(parse_window(two), 10_000);
        let set = r#"{"a":{"settings":{"index":{"max_result_window":"2000"}},
                          "defaults":{"index":{"max_result_window":"10000"}}}}"#;
        assert_eq!(parse_window(set), 2000);
        assert_eq!(parse_window("{}"), DEFAULT_WINDOW);
        assert_eq!(parse_window("not json"), DEFAULT_WINDOW);
    }

    #[test]
    fn an_error_body_gives_its_reason() {
        let root = r#"{"error":{"root_cause":[{"type":"query_shard_exception","reason":"Failed to parse query [a:]"}],"reason":"all shards failed"},"status":400}"#;
        assert_eq!(
            error_reason(root).as_deref(),
            Some("Failed to parse query [a:]")
        );
        let flat = r#"{"error":{"type":"x","reason":"no such index"}}"#;
        assert_eq!(error_reason(flat).as_deref(), Some("no such index"));
        assert_eq!(error_reason(r#"{"hits":{}}"#), None);
        assert_eq!(error_reason("<html>"), None);
    }

    #[test]
    fn only_columns_the_mapping_can_sort_get_a_sort_field() {
        let f = |name: &str, ty: &str| Field {
            name: name.into(),
            ty: ty.into(),
            nested: false,
        };
        let fields = [
            f("sku", "keyword"),
            f("price", "float"),
            f("title", "text"),
            f("title.keyword", "keyword"),
            f("body", "text"),
            f("loc", "geo_point"),
            Field {
                nested: true,
                ..f("reviews.user", "keyword")
            },
        ];
        assert_eq!(sort_field("sku", &fields).as_deref(), Some("sku"));
        assert_eq!(sort_field("price", &fields).as_deref(), Some("price"));
        assert_eq!(
            sort_field("title", &fields).as_deref(),
            Some("title.keyword")
        );
        assert_eq!(sort_field("body", &fields), None);
        assert_eq!(sort_field("loc", &fields), None);
        assert_eq!(sort_field("reviews.user", &fields), None);
        assert_eq!(sort_field("_id", &fields), None);
    }
}
