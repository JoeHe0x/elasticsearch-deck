//! Search hits as a table: `_id`, `_index`, `_score`, then the union of the top-level `_source` keys.

use serde_json::Value;

/// More columns than this are folded away; a sparse index can have thousands of fields.
const MAX_COLUMNS: usize = 60;

/// Cells longer than this are cut for display; the JSON view still has the full value.
const MAX_CELL: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hits {
    pub columns: Vec<String>,
    /// One entry per hit, aligned with `columns`.
    pub rows: Vec<Vec<String>>,
    /// `hits.total` as ES reports it, e.g. `10000+` when the count is a lower bound.
    pub total: Option<String>,
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
    let rows = list
        .iter()
        .map(|h| {
            let mut row = vec![
                cell(h.get("_id")),
                cell(h.get("_index")),
                cell(h.get("_score")),
            ];
            let src = h.get("_source");
            row.extend(keys.iter().map(|k| cell(src.and_then(|s| s.get(*k)))));
            row
        })
        .collect();
    Some(Hits {
        columns,
        rows,
        total,
    })
}

fn cell(v: Option<&Value>) -> String {
    let text = match v {
        None => return String::new(),
        Some(Value::String(s)) => s.clone(),
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
        assert_eq!(h.rows[0], ["1", "a", "1.0", "s", r#"{"b":1,"a":2}"#, ""]);
        assert_eq!(h.rows[1], ["2", "a", "null", "", "", "null"]);
        assert_eq!(h.total.as_deref(), Some("10000+"));
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
