//! Index field names for completion, taken from a `GET /{index}/_mapping` response.
//!
//! The engine never fetches anything itself: callers implement [`Fields`] over whatever mapping cache they keep.

use std::sync::Arc;

use serde_json::Value;

/// A field of the target index, addressed by its full dotted path (`user.name`, `title.keyword`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    /// The mapping `type` (`keyword`, `text`, `long`, `nested`, ...); `object` when only `properties` is given.
    pub ty: String,
    /// True when an ancestor is a `nested` field, so the field is only reachable inside a nested query.
    pub nested: bool,
}

/// What a mapping type is good for, reduced to the distinctions completion needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Keyword,
    Numeric,
    Date,
    Boolean,
    Ip,
    Version,
    /// `geo_point`, `geo_shape`, and the other spatial types.
    Geo,
    /// Objects and other types that cannot be the target of a term-level or match query.
    Object,
    Nested,
    /// A mapping type this list does not know; callers decide whether to keep it.
    Other,
}

impl Field {
    /// Classifies the mapping type. The mapping is the source of truth; this only groups the names.
    pub fn kind(&self) -> FieldKind {
        match self.ty.as_str() {
            "text" | "match_only_text" | "annotated_text" | "search_as_you_type" => FieldKind::Text,
            // `flattened` stores leaf values as keywords, so term-level queries can address it.
            "keyword" | "constant_keyword" | "wildcard" | "flattened" => FieldKind::Keyword,
            "byte" | "short" | "integer" | "long" | "unsigned_long" | "half_float" | "float"
            | "double" | "scaled_float" | "token_count" => FieldKind::Numeric,
            "date" | "date_nanos" => FieldKind::Date,
            "boolean" => FieldKind::Boolean,
            "ip" => FieldKind::Ip,
            "version" => FieldKind::Version,
            "geo_point" | "geo_shape" | "point" | "shape" => FieldKind::Geo,
            "nested" => FieldKind::Nested,
            // `alias` and `join` are queryable by name, but their target type is not in the mapping
            // entry, so they stay `Other` and only the broad clauses take them.
            "alias" | "join" => FieldKind::Other,
            "object" | "completion" | "dense_vector" | "sparse_vector" | "rank_feature"
            | "rank_features" | "percolator" => FieldKind::Object,
            _ => FieldKind::Other,
        }
    }
}

/// Source of field names for the index expression in a request path.
pub trait Fields {
    /// `target` is the path segment standing for the index (`logs-*`, `a,b`, an alias), or `""` when the path has none.
    ///
    /// Shared rather than owned because completion runs on every keystroke: a cache hands out the
    /// same table without copying its strings.
    fn fields(&self, target: &str) -> Arc<[Field]>;

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
    fn fields(&self, _target: &str) -> Arc<[Field]> {
        Arc::from([])
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
            walk(props, "", false, &mut out);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

fn walk(props: &Value, prefix: &str, in_nested: bool, out: &mut Vec<Field>) {
    let Some(props) = props.as_object() else {
        return;
    };
    for (name, def) in props {
        let full = format!("{prefix}{name}");
        let ty = def.get("type").and_then(Value::as_str).unwrap_or("object");
        let nested = in_nested || ty == "nested";
        out.push(Field {
            name: full.clone(),
            ty: ty.to_string(),
            // The `nested` field itself is declared on the enclosing object; its children need a nested query.
            nested: in_nested,
        });
        let child_prefix = format!("{full}.");
        for key in ["properties", "fields"] {
            if let Some(children) = def.get(key) {
                walk(children, &child_prefix, nested, out);
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
        let fields = parse_mapping(json);
        let got: Vec<(String, String)> = fields
            .iter()
            .map(|f| (f.name.clone(), f.ty.clone()))
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
        let nested: Vec<(&str, bool)> =
            fields.iter().map(|f| (f.name.as_str(), f.nested)).collect();
        assert_eq!(
            nested,
            [
                ("n", false),
                // The nested field itself is declared on the parent; only its children need a nested query.
                ("tags", false),
                ("tags.k", true),
                ("title", false),
                ("title.keyword", false),
                ("user", false),
                ("user.name", false),
            ]
        );
    }

    #[test]
    fn classifies_mapping_types() {
        let kind = |ty: &str| {
            Field {
                name: "f".into(),
                ty: ty.into(),
                nested: false,
            }
            .kind()
        };
        use FieldKind::*;
        let cases = [
            ("text", Text),
            ("match_only_text", Text),
            ("search_as_you_type", Text),
            ("keyword", Keyword),
            ("constant_keyword", Keyword),
            ("wildcard", Keyword),
            ("long", Numeric),
            ("scaled_float", Numeric),
            ("date", Date),
            ("date_nanos", Date),
            ("boolean", Boolean),
            ("ip", Ip),
            ("version", Version),
            ("geo_point", Geo),
            ("geo_shape", Geo),
            ("nested", Nested),
            ("object", Object),
            ("flattened", Keyword),
            ("alias", Other),
            ("join", Other),
            ("completion", Object),
            ("dense_vector", Object),
            ("something_new", Other),
        ];
        for (ty, want) in cases {
            assert_eq!(kind(ty), want, "{ty}");
        }
    }

    #[test]
    fn garbage_gives_no_fields() {
        for s in ["", "[]", "{", "{\"a\": 1}", "{\"a\": {\"mappings\": []}}"] {
            assert!(parse_mapping(s).is_empty(), "{s:?}");
        }
    }
}
