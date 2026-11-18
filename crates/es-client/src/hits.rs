//! Search hits as a table: `_id`, `_index`, `_score`, then the union of the top-level `_source` keys.

use serde_json::Value;

/// More columns than this are folded away; a sparse index can have thousands of fields.
const MAX_COLUMNS: usize = 60;

/// Cells longer than this are cut for display; the JSON view still has the full value.
const MAX_CELL: usize = 400;

/// What a cell held before it was turned into text, so a view can style it without re-parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    /// The key is absent from this hit's `_source`.
    Missing,
    Null,
    Bool,
    Number,
    Text,
    /// An array and its length.
    Array(usize),
    Object,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hits {
    pub columns: Vec<String>,
    /// One entry per hit, aligned with `columns`.
    pub rows: Vec<Vec<String>>,
    /// The kind of every cell in `rows`.
    pub kinds: Vec<Vec<CellKind>>,
    /// `hits.total` as ES reports it, e.g. `10000+` when the count is a lower bound.
    pub total: Option<String>,
    /// The `took` ES reports (ms spent inside the cluster), to tell it apart from network time.
    pub took_ms: Option<u64>,
    /// Every hit, pretty-printed, for a detail view and for copying.
    pub docs: Vec<String>,
}

/// Reads `hits.hits` from a search response; `None` when the body is not one.
pub fn parse_hits(body: &str) -> Option<Hits> {
    let json: Value = serde_json::from_str(body).ok()?;
    let hits = json.get("hits")?;
    let list = hits.get("hits")?.as_array()?;
    let total = match hits.get("total") {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(t) => t.get("value").map(|v| {
            let plus = if t.get("relation").and_then(Value::as_str) == Some("gte") {
                "+"
            } else {
                ""
            };
            format!("{v}{plus}")
        }),
        None => None,
    };
    let mut keys: Vec<&str> = Vec::new();
    for h in list {
        if let Some(src) = h.get("_source").and_then(Value::as_object) {
            for k in src.keys() {
                if keys.len() < MAX_COLUMNS && !keys.contains(&k.as_str()) {
                    keys.push(k);
                }
            }
        }
    }
    let columns = ["_id", "_index", "_score"]
        .into_iter()
        .chain(keys.iter().copied())
        .map(String::from)
        .collect();
    let (rows, kinds) = list
        .iter()
        .map(|h| {
            let src = h.get("_source");
            ["_id", "_index", "_score"]
                .into_iter()
                .map(|k| h.get(k))
                .chain(keys.iter().map(|k| src.and_then(|s| s.get(*k))))
                .map(|v| (cell(v), kind(v)))
                .unzip::<_, _, Vec<_>, Vec<_>>()
        })
        .unzip();
    Some(Hits {
        columns,
        rows,
        kinds,
        total,
        took_ms: json.get("took").and_then(Value::as_u64),
        docs: list
            .iter()
            .map(|h| serde_json::to_string_pretty(h).unwrap_or_default())
            .collect(),
    })
}

fn kind(v: Option<&Value>) -> CellKind {
    match v {
        None => CellKind::Missing,
        Some(Value::Null) => CellKind::Null,
        Some(Value::Bool(_)) => CellKind::Bool,
        Some(Value::Number(_)) => CellKind::Number,
        Some(Value::String(_)) => CellKind::Text,
        Some(Value::Array(a)) => CellKind::Array(a.len()),
        Some(Value::Object(_)) => CellKind::Object,
    }
}

/// How many items of an array (or keys of an object) a cell names before `+N`.
const SUMMARY_ITEMS: usize = 3;

fn item(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn summary(items: impl ExactSizeIterator<Item = String>) -> String {
    let total = items.len();
    let shown: Vec<String> = items.take(SUMMARY_ITEMS).collect();
    match total.saturating_sub(SUMMARY_ITEMS) {
        0 => shown.join(", "),
        rest => format!("{} +{rest}", shown.join(", ")),
    }
}

fn cell(v: Option<&Value>) -> String {
    let text = match v {
        None => return String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => summary(a.iter().map(item)),
        // The keys say more about an object than the start of its JSON; the detail view has the rest.
        Some(Value::Object(o)) => format!("{{{}}}", summary(o.keys().cloned())),
        Some(other) => other.to_string(),
    };
    match text.char_indices().nth(MAX_CELL) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_columns_from_the_union_of_source_keys() {
        let body = r#"{"hits":{"total":{"value":10000,"relation":"gte"},"hits":[
            {"_index":"a","_id":"1","_score":1.0,"_source":{"x":"s","n":{"b":1,"a":2}}},
            {"_index":"a","_id":"2","_score":null,"_source":{"y":null}}]}}"#;
        let h = parse_hits(body).unwrap();
        assert_eq!(h.columns, ["_id", "_index", "_score", "x", "n", "y"]);
        assert_eq!(h.rows[0], ["1", "a", "1.0", "s", "{b, a}", ""]);
        assert_eq!(h.rows[1], ["2", "a", "null", "", "", "null"]);
        assert_eq!(h.total.as_deref(), Some("10000+"));
        assert_eq!(
            h.kinds[0],
            [
                CellKind::Text,
                CellKind::Text,
                CellKind::Number,
                CellKind::Text,
                CellKind::Object,
                CellKind::Missing
            ]
        );
        assert_eq!(h.kinds[1][2], CellKind::Null);
    }

    #[test]
    fn arrays_keep_their_length() {
        let body =
            r#"{"took":7,"hits":{"total":1,"hits":[{"_id":"1","_source":{"a":["x","y"]}}]}}"#;
        let h = parse_hits(body).unwrap();
        assert_eq!(h.kinds[0][3], CellKind::Array(2));
        assert_eq!(h.took_ms, Some(7));
        assert_eq!(h.rows[0][3], "x, y");
        let many = parse_hits(r#"{"hits":{"hits":[{"_source":{"a":[1,2,3,4,5]}}]}}"#).unwrap();
        assert_eq!(many.rows[0][3], "1, 2, 3 +2");
        assert!(h.docs[0].contains("\"_id\": \"1\""));
    }

    #[test]
    fn accepts_a_plain_total_and_rejects_other_bodies() {
        let h = parse_hits(r#"{"hits":{"total":3,"hits":[]}}"#).unwrap();
        assert_eq!((h.total.as_deref(), h.rows.len()), (Some("3"), 0));
        assert!(parse_hits(r#"{"acknowledged":true}"#).is_none());
        assert!(parse_hits("not json").is_none());
    }

    #[test]
    fn long_cells_are_cut_on_a_char_boundary() {
        let long = "é".repeat(MAX_CELL + 5);
        let shown = cell(Some(&Value::String(long)));
        assert_eq!(shown.chars().count(), MAX_CELL + 1);
    }
}
