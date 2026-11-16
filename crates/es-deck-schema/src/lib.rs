//! Converts `elasticsearch-specification`'s `schema.json` into the compact [`Index`].
//!
//! Limitations of this cut: only `interface` types and plain-property request bodies are indexed;
//! `behaviors` / `attachedBehaviors` (e.g. `AdditionalProperty` field-name keys) are ignored,
//! and generics on `inherits` are dropped. Availability is kept only as `stack.since`.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use es_dsl_core::{Endpoint, Index, OpenKey, Prop, TypeRef, Url, Version, parse_version};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Spec {
    endpoints: Vec<SpecEndpoint>,
    types: Vec<SpecType>,
}

#[derive(Deserialize)]
struct SpecEndpoint {
    name: String,
    urls: Vec<SpecUrl>,
    request: Option<Name>,
    availability: Option<Availability>,
    #[serde(default, rename = "requestMediaType")]
    request_media_type: Vec<String>,
}

#[derive(Deserialize)]
struct Availability {
    stack: Option<StackAvailability>,
}

#[derive(Deserialize)]
struct StackAvailability {
    since: Option<String>,
}

impl Availability {
    fn since(a: &Option<Self>) -> Option<Version> {
        a.as_ref()?
            .stack
            .as_ref()?
            .since
            .as_deref()
            .and_then(parse_version)
    }
}

#[derive(Deserialize)]
struct SpecUrl {
    path: String,
    methods: Vec<String>,
}

#[derive(Deserialize, Clone)]
struct Name {
    name: String,
    namespace: String,
}

impl Name {
    fn full(&self) -> String {
        format!("{}.{}", self.namespace, self.name)
    }
}

#[derive(Deserialize)]
struct SpecType {
    kind: String,
    name: Name,
    #[serde(default)]
    properties: Vec<SpecProp>,
    body: Option<SpecBody>,
    #[serde(default)]
    query: Vec<SpecProp>,
    #[serde(default, rename = "attachedBehaviors")]
    attached_behaviors: Vec<String>,
    inherits: Option<SpecInherits>,
    #[serde(default)]
    members: Vec<SpecMember>,
    /// For `type_alias`: the aliased type.
    #[serde(rename = "type")]
    alias_of: Option<Value>,
    #[serde(default)]
    generics: Vec<Value>,
    #[serde(default)]
    behaviors: Vec<SpecBehavior>,
}

#[derive(Deserialize)]
struct SpecBehavior {
    #[serde(rename = "type")]
    ty: Name,
    #[serde(default)]
    generics: Vec<Value>,
}

/// The open key of `t` or its nearest ancestor that has one: `AdditionalProperty<K, V>` (and the plural form)
/// means any key of type `K` maps to a `V`.
fn open_key(
    types: &HashMap<String, &SpecType>,
    t: &SpecType,
    aliases: &Aliases,
    seen: &mut HashSet<String>,
) -> Option<OpenKey> {
    let own = t
        .behaviors
        .iter()
        .filter(|b| {
            b.ty.namespace == "_spec_utils"
                && matches!(
                    b.ty.name.as_str(),
                    "AdditionalProperty" | "AdditionalProperties"
                )
        })
        .find_map(|b| {
            let [key, value] = b.generics.as_slice() else {
                return None;
            };
            let is_field = serde_json::from_value::<Name>(key.get("type")?.clone())
                .is_ok_and(|n| n.namespace == "_types" && n.name == "Field");
            Some(OpenKey {
                is_field,
                value: type_ref(value, aliases, 0),
            })
        });
    own.or_else(|| {
        let parent = t.inherits.as_ref().and_then(|i| {
            let name = i.ty.full();
            seen.insert(name.clone())
                .then(|| types.get(&name))
                .flatten()
        })?;
        open_key(types, parent, aliases, seen)
    })
}

/// Non-generic type aliases by full name, expanded inline so completion never sees them.
type Aliases<'a> = HashMap<String, &'a Value>;

/// Alias nesting deeper than this is treated as unknown; real spec chains are far shorter.
const MAX_ALIAS_DEPTH: u8 = 8;

#[derive(Deserialize)]
struct SpecMember {
    name: String,
}

#[derive(Deserialize)]
struct SpecInherits {
    #[serde(rename = "type")]
    ty: Name,
}

#[derive(Deserialize)]
struct SpecBody {
    kind: String,
    #[serde(default)]
    properties: Vec<SpecProp>,
    /// For `kind == "value"`.
    value: Option<Value>,
}

#[derive(Deserialize)]
struct SpecProp {
    name: String,
    #[serde(rename = "type")]
    ty: Value,
    #[serde(default)]
    aliases: Vec<String>,
    availability: Option<Availability>,
    description: Option<String>,
    #[serde(rename = "serverDefault")]
    server_default: Option<Value>,
    deprecation: Option<Value>,
}

/// Longest description kept per property; the index repeats it for every type that inherits the property.
const MAX_DOC: usize = 240;

/// The first paragraph of a spec description, cut at a word boundary; `None` when it is blank.
fn brief(desc: &str) -> Option<String> {
    let para = desc.split("\n\n").next()?.trim();
    if para.is_empty() {
        return None;
    }
    if para.chars().count() <= MAX_DOC {
        return Some(para.into());
    }
    let cut: String = para.chars().take(MAX_DOC).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    Some(format!("{cut}..."))
}

fn type_ref(v: &Value, aliases: &Aliases, depth: u8) -> TypeRef {
    let nested = |key: &str| {
        v.get(key)
            .map_or(TypeRef::Other, |t| type_ref(t, aliases, depth))
    };
    match v.get("kind").and_then(Value::as_str) {
        Some("instance_of") => v
            .get("type")
            .and_then(|t| serde_json::from_value::<Name>(t.clone()).ok())
            .map_or(TypeRef::Other, |n| {
                if n.namespace == "_builtins" && n.name == "boolean" {
                    TypeRef::Bool
                } else if n.namespace == "_types" && n.name == "Field" {
                    TypeRef::Field
                } else if let Some(target) = aliases.get(&n.full()) {
                    if depth < MAX_ALIAS_DEPTH {
                        type_ref(target, aliases, depth + 1)
                    } else {
                        TypeRef::Other
                    }
                } else {
                    TypeRef::Named(n.full())
                }
            }),
        Some("literal_value") => v
            .get("value")
            .map_or(TypeRef::Other, |l| TypeRef::Literal(l.to_string())),
        Some("array_of") => TypeRef::Array(Box::new(nested("value"))),
        Some("dictionary_of") => {
            let value = Box::new(nested("value"));
            match v.get("key").map(|k| type_ref(k, aliases, depth)) {
                Some(TypeRef::Field) => TypeRef::FieldDict(value),
                _ => TypeRef::Dict(value),
            }
        }
        Some("union_of") => TypeRef::Union(
            v.get("items")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|t| type_ref(t, aliases, depth)).collect())
                .unwrap_or_default(),
        ),
        _ => TypeRef::Other,
    }
}

/// A property plus one copy per alias (e.g. `aggregations` / `aggs`): aliases are just other accepted keys.
fn props(src: &[SpecProp], aliases: &Aliases) -> Vec<Prop> {
    src.iter()
        .flat_map(|p| {
            std::iter::once(&p.name).chain(&p.aliases).map(|name| Prop {
                since: Availability::since(&p.availability),
                doc: p.description.as_deref().and_then(brief),
                default: p.server_default.as_ref().map(Value::to_string),
                deprecated: p.deprecation.is_some(),
                ..Prop::new(name.clone(), type_ref(&p.ty, aliases, 0))
            })
        })
        .collect()
}

/// Own properties followed by inherited ones; a child property hides a parent's of the same name.
/// `seen` guards against inheritance cycles.
fn flatten(
    types: &HashMap<String, &SpecType>,
    t: &SpecType,
    aliases: &Aliases,
    seen: &mut HashSet<String>,
) -> Vec<Prop> {
    let mut out = props(&t.properties, aliases);
    if let Some(parent) = t.inherits.as_ref().and_then(|i| {
        let name = i.ty.full();
        seen.insert(name.clone())
            .then(|| types.get(&name))
            .flatten()
    }) {
        for p in flatten(types, parent, aliases, seen) {
            if !out.iter().any(|o| o.name == p.name) {
                out.push(p);
            }
        }
    }
    out
}

/// Builds the index from the text of `schema.json`.
pub fn convert(spec_json: &str) -> Result<Index> {
    let spec: Spec = serde_json::from_str(spec_json).context("parsing schema.json")?;
    let mut index = Index::default();
    let aliases: Aliases = spec
        .types
        .iter()
        .filter(|t| t.kind == "type_alias" && t.generics.is_empty())
        .filter_map(|t| Some((t.name.full(), t.alias_of.as_ref()?)))
        .collect();
    let interfaces: HashMap<String, &SpecType> = spec
        .types
        .iter()
        .filter(|t| t.kind == "interface")
        .map(|t| (t.name.full(), t))
        .collect();
    for (name, t) in &interfaces {
        let mut seen = HashSet::from([name.clone()]);
        index
            .types
            .insert(name.clone(), flatten(&interfaces, t, &aliases, &mut seen));
        let mut seen = HashSet::from([name.clone()]);
        if let Some(open) = open_key(&interfaces, t, &aliases, &mut seen) {
            index.open.insert(name.clone(), open);
        }
    }
    for t in spec.types.iter().filter(|t| t.kind == "enum") {
        index.enums.insert(
            t.name.full(),
            t.members.iter().map(|m| m.name.clone()).collect(),
        );
    }
    let requests: HashMap<String, &SpecType> = spec
        .types
        .iter()
        .filter(|t| t.kind == "request")
        .map(|t| (t.name.full(), t))
        .collect();
    for e in &spec.endpoints {
        let body = e
            .request
            .as_ref()
            .and_then(|r| requests.get(&r.full()))
            .and_then(|t| t.body.as_ref())
            .filter(|b| b.kind == "properties")
            .map(|b| props(&b.properties, &aliases))
            .unwrap_or_default();
        let request = e.request.as_ref().and_then(|r| requests.get(&r.full()));
        let value_body = request
            .and_then(|t| t.body.as_ref())
            .filter(|b| b.kind == "value")
            .and_then(|b| b.value.as_ref())
            .map(|v| type_ref(v, &aliases, 0));
        let is_ndjson = e
            .request_media_type
            .iter()
            .any(|m| m == "application/x-ndjson");
        let (body_ty, lines) = match (value_body, is_ndjson) {
            (Some(TypeRef::Array(el)), true) => (
                None,
                match *el {
                    TypeRef::Union(items) => items,
                    one => vec![one],
                },
            ),
            (body, _) => (body, Vec::new()),
        };
        // Behaviors such as `CommonQueryParameters` add URL-level parameters to every request that attaches them.
        let query = request.iter().flat_map(|t| {
            let common = t
                .attached_behaviors
                .iter()
                .filter_map(|b| interfaces.get(&format!("_spec_utils.{b}")))
                .flat_map(|b| props(&b.properties, &aliases));
            props(&t.query, &aliases).into_iter().chain(common)
        });
        index.endpoints.push(Endpoint {
            name: e.name.clone(),
            urls: e
                .urls
                .iter()
                .map(|u| Url {
                    path: u.path.clone(),
                    methods: u.methods.clone(),
                })
                .collect(),
            body,
            body_ty,
            lines,
            query: query.collect(),
            since: Availability::since(&e.availability),
        });
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"{
      "endpoints": [{
        "name": "search",
        "urls": [{"path": "/{index}/_search", "methods": ["GET", "POST"]}],
        "request": {"name": "Request", "namespace": "_global.search"},
        "availability": {"stack": {"since": "7.0.0"}, "serverless": {}}
      }, {
        "name": "ping", "urls": [{"path": "/", "methods": ["HEAD"]}], "request": null
      }],
      "types": [
        {"kind": "request", "name": {"name": "Request", "namespace": "_global.search"},
         "attachedBehaviors": ["CommonQueryParameters"],
         "query": [{"name": "preference", "type": {"kind": "user_defined_value"}}],
         "body": {"kind": "properties", "properties": [
           {"name": "query", "type": {"kind": "instance_of",
             "type": {"name": "QueryContainer", "namespace": "_types.query_dsl"}}},
           {"name": "aggregations", "availability": {"stack": {"since": "8.11"}}, "type": {"kind": "dictionary_of",
             "key": {"kind": "instance_of", "type": {"name": "string", "namespace": "_builtins"}},
             "value": {"kind": "instance_of", "type": {"name": "Agg", "namespace": "_types"}}}}
         ]}},
        {"kind": "interface", "name": {"name": "QueryContainer", "namespace": "_types.query_dsl"},
         "properties": [
           {"name": "bool", "type": {"kind": "instance_of",
             "type": {"name": "BoolQuery", "namespace": "_types.query_dsl"}}},
           {"name": "ids", "type": {"kind": "array_of", "value": {"kind": "literal_value", "value": 1}}},
           {"name": "u", "type": {"kind": "union_of", "items": [
             {"kind": "user_defined_value"}, {"kind": "instance_of",
              "type": {"name": "X", "namespace": "_types"}}]}}
         ]},
        {"kind": "interface", "name": {"name": "Base", "namespace": "_types"},
         "inherits": {"type": {"name": "Cyclic", "namespace": "_types"}},
         "properties": [{"name": "boost", "type": {"kind": "user_defined_value"}},
                        {"name": "ids", "type": {"kind": "user_defined_value"}}]},
        {"kind": "interface", "name": {"name": "Cyclic", "namespace": "_types"},
         "inherits": {"type": {"name": "Base", "namespace": "_types"}}, "properties": []},
        {"kind": "interface", "name": {"name": "Child", "namespace": "_types"},
         "inherits": {"type": {"name": "Base", "namespace": "_types"}},
         "properties": [{"name": "ids", "aliases": ["id_list"], "type": {"kind": "instance_of",
           "type": {"name": "X", "namespace": "_types"}}}]},
        {"kind": "interface", "name": {"name": "CommonQueryParameters", "namespace": "_spec_utils"},
         "properties": [{"name": "pretty", "type": {"kind": "user_defined_value"}}]},
        {"kind": "enum", "name": {"name": "E", "namespace": "_types"}}
      ]
    }"#;

    #[test]
    fn converts_endpoints_and_interfaces() {
        let idx = convert(SPEC).unwrap();
        let search = &idx.endpoints[0];
        assert_eq!(search.urls[0].path, "/{index}/_search");
        assert_eq!(search.body[0].name, "query");
        assert_eq!(
            search.body[0].ty,
            TypeRef::Named("_types.query_dsl.QueryContainer".into())
        );
        assert_eq!(
            search.body[1].ty,
            TypeRef::Dict(Box::new(TypeRef::Named("_types.Agg".into())))
        );
        assert!(idx.endpoints[1].body.is_empty());
        assert_eq!(search.since, Some([7, 0, 0]));
        assert_eq!(idx.endpoints[1].since, None);
        assert_eq!(search.body[0].since, None);
        assert_eq!(search.body[1].since, Some([8, 11, 0]));
        let q = &idx.types["_types.query_dsl.QueryContainer"];
        assert_eq!(
            q[1].ty,
            TypeRef::Array(Box::new(TypeRef::Literal("1".into())))
        );
        assert_eq!(
            q[2].ty,
            TypeRef::Union(vec![TypeRef::Other, TypeRef::Named("_types.X".into())])
        );
        assert_eq!(idx.types.len(), 5);
        let query: Vec<_> = search.query.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(query, ["preference", "pretty"]);
    }

    #[test]
    fn expands_type_aliases() {
        let spec = r#"{
          "endpoints": [],
          "types": [
            {"kind": "type_alias", "name": {"name": "A", "namespace": "n"},
             "type": {"kind": "union_of", "items": [
               {"kind": "instance_of", "type": {"name": "E", "namespace": "n"}},
               {"kind": "instance_of", "type": {"name": "A", "namespace": "n"}}]}},
            {"kind": "type_alias", "name": {"name": "G", "namespace": "n"}, "generics": [{}],
             "type": {"kind": "instance_of", "type": {"name": "E", "namespace": "n"}}},
            {"kind": "interface", "name": {"name": "T", "namespace": "n"}, "properties": [
              {"name": "a", "type": {"kind": "instance_of", "type": {"name": "A", "namespace": "n"}}},
              {"name": "g", "type": {"kind": "instance_of", "type": {"name": "G", "namespace": "n"}}}]}
          ]
        }"#;
        let t = &convert(spec).unwrap().types["n.T"];
        // the self-referencing alias stops at the depth limit instead of looping
        assert!(matches!(&t[0].ty, TypeRef::Union(v) if v[0] == TypeRef::Named("n.E".into())));
        // generic aliases are left as plain names
        assert_eq!(t[1].ty, TypeRef::Named("n.G".into()));
    }

    #[test]
    fn indexes_open_keys_through_inheritance() {
        let spec = r#"{
          "endpoints": [],
          "types": [
            {"kind": "interface", "name": {"name": "Base", "namespace": "n"}, "properties": [],
             "behaviors": [{"type": {"name": "AdditionalProperty", "namespace": "_spec_utils"},
               "generics": [
                 {"kind": "instance_of", "type": {"name": "Field", "namespace": "_types"}},
                 {"kind": "instance_of", "type": {"name": "V", "namespace": "n"}}]}]},
            {"kind": "interface", "name": {"name": "Child", "namespace": "n"}, "properties": [],
             "inherits": {"type": {"name": "Base", "namespace": "n"}}},
            {"kind": "interface", "name": {"name": "Plain", "namespace": "n"}, "properties": []}
          ]
        }"#;
        let idx = convert(spec).unwrap();
        let want = OpenKey {
            is_field: true,
            value: TypeRef::Named("n.V".into()),
        };
        assert_eq!(idx.open["n.Base"], want);
        assert_eq!(idx.open["n.Child"], want);
        assert!(!idx.open.contains_key("n.Plain"));
    }

    #[test]
    fn recognizes_field_names_and_field_keyed_dictionaries() {
        let field = r#"{"kind": "instance_of", "type": {"name": "Field", "namespace": "_types"}}"#;
        let spec = format!(
            r#"{{"endpoints": [], "types": [
              {{"kind": "type_alias", "name": {{"name": "Field", "namespace": "_types"}},
                "type": {{"kind": "instance_of", "type": {{"name": "string", "namespace": "_builtins"}}}}}},
              {{"kind": "interface", "name": {{"name": "T", "namespace": "n"}}, "properties": [
                {{"name": "f", "type": {field}}},
                {{"name": "d", "type": {{"kind": "dictionary_of", "key": {field}, "value": {field}}}}},
                {{"name": "s", "type": {{"kind": "dictionary_of", "key": {{"kind": "instance_of",
                  "type": {{"name": "string", "namespace": "_builtins"}}}}, "value": {field}}}}}]}}
            ]}}"#
        );
        let t = &convert(&spec).unwrap().types["n.T"];
        assert_eq!(t[0].ty, TypeRef::Field);
        assert_eq!(t[1].ty, TypeRef::FieldDict(Box::new(TypeRef::Field)));
        assert_eq!(t[2].ty, TypeRef::Dict(Box::new(TypeRef::Field)));
    }

    #[test]
    fn indexes_ndjson_and_value_bodies() {
        let req = |name: &str, body: &str| {
            format!(
                r#"{{"kind": "request", "name": {{"name": "{name}", "namespace": "r"}}, "body": {body}}}"#
            )
        };
        let spec = format!(
            r#"{{
              "endpoints": [
                {{"name": "bulk", "urls": [], "request": {{"name": "B", "namespace": "r"}},
                  "requestMediaType": ["application/x-ndjson"]}},
                {{"name": "put", "urls": [], "request": {{"name": "P", "namespace": "r"}},
                  "requestMediaType": ["application/json"]}}
              ],
              "types": [{b}, {p}]
            }}"#,
            b = req(
                "B",
                r#"{"kind": "value", "value": {"kind": "array_of", "value": {"kind": "union_of", "items": [
                    {"kind": "instance_of", "type": {"name": "A", "namespace": "n"}},
                    {"kind": "instance_of", "type": {"name": "U", "namespace": "n"}}]}}}"#
            ),
            p = req(
                "P",
                r#"{"kind": "value", "value": {"kind": "instance_of", "type": {"name": "S", "namespace": "n"}}}"#
            ),
        );
        let idx = convert(&spec).unwrap();
        assert_eq!(
            idx.endpoints[0].lines,
            [TypeRef::Named("n.A".into()), TypeRef::Named("n.U".into())]
        );
        assert_eq!(idx.endpoints[0].body_ty, None);
        assert!(idx.endpoints[1].lines.is_empty());
        assert_eq!(idx.endpoints[1].body_ty, Some(TypeRef::Named("n.S".into())));
    }

    #[test]
    fn merges_inherits_and_aliases() {
        let idx = convert(SPEC).unwrap();
        let child: Vec<_> = idx.types["_types.Child"]
            .iter()
            .map(|p| (p.name.as_str(), &p.ty))
            .collect();
        let x = TypeRef::Named("_types.X".into());
        // own `ids` (and its alias) hide the parent's `ids`; `boost` is inherited; the cycle terminates
        assert_eq!(
            child,
            [("ids", &x), ("id_list", &x), ("boost", &TypeRef::Other)]
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(convert("{").is_err());
    }
}
