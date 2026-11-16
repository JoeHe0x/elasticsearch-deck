//! Index field names for completion, taken from a `GET /{index}/_mapping` response.
//!
//! The engine never fetches anything itself: callers implement [`Fields`] over whatever mapping cache they keep.

use serde_json::Value;

/// A field of the target index, addressed by its full dotted path (`user.name`, `title.keyword`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    /// The mapping `type` (`keyword`, `text`, `long`, `nested`, ...); `object` when only `properties` is given.
    pub ty: String,
}

/// Source of field names for the index expression in a request path.
pub trait Fields {
    /// `target` is the path segment standing for the index (`logs-*`, `a,b`, an alias), or `""` when the path has none.
    fn fields(&self, target: &str) -> Vec<Field>;

    /// Index and alias names offered for the `{index}` URL segment.
    fn names(&self) -> Vec<String> {
        Vec::new()
    }

    /// The alias names among [`Fields::names`], for callers that list the two kinds apart.
    fn aliases(&self) -> Vec<String> {
        Vec::new()
    }
}

/// No mapping available.
pub struct NoFields;

impl Fields for NoFields {
    fn fields(&self, _target: &str) -> Vec<Field> {
        Vec::new()
    }
}

/// Index names from `GET _cat/indices?format=json` and alias names from `GET _alias`, deduplicated.
/// Either input may be empty or malformed; it then contributes nothing.
pub fn parse_names(cat_indices: &str, aliases: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(cat_indices) {
        out.extend(
            rows.iter()
                .filter_map(|r| r["index"].as_str().map(String::from)),
        );
    }
    if let Ok(Value::Object(root)) = serde_json::from_str::<Value>(aliases) {
        for (index, v) in &root {
            out.push(index.clone());
            if let Some(Value::Object(a)) = v.get("aliases") {
                out.extend(a.keys().cloned());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Alias names from a `GET _alias` response, sorted and deduplicated; malformed input gives none.
pub fn parse_aliases(aliases: &str) -> Vec<String> {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(aliases) else {
        return Vec::new();
    };
    let mut out: Vec<String> = root
        .values()
        .filter_map(|v| v.get("aliases")?.as_object())
        .flat_map(|a| a.keys().cloned())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Flattens a `_mapping` response (`{index: {mappings: {properties: ...}}}`) into fields, merged across indices.
/// Object and nested fields are listed too, as are multi-fields (`fields`). Anything else in the input is ignored.
pub fn parse_mapping(json: &str) -> Vec<Field> {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for index in root.values() {
        if let Some(props) = index.pointer("/mappings/properties") {
            walk(props, "", &mut out);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

fn walk(props: &Value, prefix: &str, out: &mut Vec<Field>) {
    let Some(props) = props.as_object() else {
        return;
    };
    for (name, def) in props {
        let full = format!("{prefix}{name}");
        let ty = def.get("type").and_then(Value::as_str).unwrap_or("object");
        out.push(Field {
            name: full.clone(),
            ty: ty.to_string(),
        });
        let child_prefix = format!("{full}.");
        for key in ["properties", "fields"] {
            if let Some(children) = def.get(key) {
                walk(children, &child_prefix, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn aliases_are_listed_apart_from_indices() {
        let json =
            r#"{"a-1":{"aliases":{"b":{},"a":{}}},"a-2":{"aliases":{"a":{}}},"c":{"aliases":{}}}"#;
        assert_eq!(super::parse_aliases(json), ["a", "b"]);
        assert!(super::parse_aliases("nope").is_empty());
    }

    use super::*;

    #[test]
    fn names_merge_indices_and_aliases() {
        let cat = r#"[{"index": "b"}, {"index": "a"}]"#;
        let alias = r#"{"a": {"aliases": {"all": {}}}, "c": {"aliases": {}}}"#;
        assert_eq!(parse_names(cat, alias), ["a", "all", "b", "c"]);
        assert!(parse_names("x", "").is_empty());
    }

    #[test]
    fn flattens_objects_nested_and_multi_fields() {
        let json = r#"{
          "a": {"mappings": {"properties": {
            "title": {"type": "text", "fields": {"keyword": {"type": "keyword"}}},
            "user": {"properties": {"name": {"type": "keyword"}}},
            "tags": {"type": "nested", "properties": {"k": {"type": "keyword"}}}}}},
          "b": {"mappings": {"properties": {"title": {"type": "text"}, "n": {"type": "long"}}}}
        }"#;
        let got: Vec<(String, String)> = parse_mapping(json)
            .into_iter()
            .map(|f| (f.name, f.ty))
            .collect();
        let want = [
            ("n", "long"),
            ("tags", "nested"),
            ("tags.k", "keyword"),
            ("title", "text"),
            ("title.keyword", "keyword"),
            ("user", "object"),
            ("user.name", "keyword"),
        ];
        assert_eq!(got, want.map(|(a, b)| (a.to_string(), b.to_string())));
    }

    #[test]
    fn garbage_gives_no_fields() {
        for s in ["", "[]", "{", "{\"a\": 1}", "{\"a\": {\"mappings\": []}}"] {
            assert!(parse_mapping(s).is_empty(), "{s:?}");
        }
    }
}
