//! Compact schema index consumed by the completion engine.
//!
//! Produced at build time by `es-deck-schema` from `elasticsearch-specification`'s `schema.json`.
//! Type names are the spec's `namespace.name`, e.g. `_types.query_dsl.QueryContainer`.

use std::collections::BTreeMap;
use std::ops::Not as _;

use serde::{Deserialize, Serialize};

/// A stack version as `[major, minor, patch]`; arrays compare lexicographically.
pub type Version = [u32; 3];

/// Parses `8.11` or `8.11.2`; missing parts are 0. `None` for anything non-numeric.
pub fn parse_version(s: &str) -> Option<Version> {
    let mut out = [0; 3];
    let mut parts = s.split('.');
    for slot in &mut out {
        match parts.next() {
            Some(p) => *slot = p.parse().ok()?,
            None => break,
        }
    }
    parts.next().is_none().then_some(out)
}

/// Whether something introduced in `since` exists on `cluster`. Unknown on either side means visible.
pub(crate) fn visible(since: Option<Version>, cluster: Option<Version>) -> bool {
    matches!((since, cluster), (Some(s), Some(c)) if s > c).not()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub endpoints: Vec<Endpoint>,
    /// Object-like types (spec `interface`s) by full name.
    pub types: BTreeMap<String, Vec<Prop>>,
    /// Enum member names (their JSON values) by full type name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub enums: BTreeMap<String, Vec<String>>,
    /// Types that also accept user-chosen keys (spec `AdditionalProperty(ies)` behavior), by full name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub open: BTreeMap<String, OpenKey>,
}

/// Extra keys an object type accepts besides its declared properties, e.g. `terms.<field>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenKey {
    /// The key names a field of the target index (spec `_types.Field`), so mapping can supply candidates.
    pub is_field: bool,
    /// Type of the value under such a key.
    pub value: TypeRef,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub name: String,
    pub urls: Vec<Url>,
    /// Top-level request body properties; empty when the body is not a plain property set.
    pub body: Vec<Prop>,
    /// Body that is one value type instead of a property set (e.g. `PUT _settings` takes `IndexSettings`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_ty: Option<TypeRef>,
    /// NDJSON body: the line types, in the spec's element union order. Empty for ordinary JSON bodies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<TypeRef>,
    /// Query-string parameters, including the common ones (`pretty`, `human`, ...). Only names matter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query: Vec<Prop>,
    /// Stack version that introduced the endpoint (spec `availability.stack.since`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Version>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Url {
    /// Path template such as `/{index}/_search`.
    pub path: String,
    pub methods: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prop {
    pub name: String,
    pub ty: TypeRef,
    /// Stack version that introduced the property. The spec records no removals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Version>,
    /// First paragraph of the spec description, shortened; shown on hover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// The server-side default (spec `serverDefault`) as JSON text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deprecated: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Prop {
    /// A property with no documentation attached.
    pub fn new(name: impl Into<String>, ty: TypeRef) -> Self {
        Self {
            name: name.into(),
            ty,
            since: None,
            doc: None,
            default: None,
            deprecated: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeRef {
    /// A spec type by full name; builtins (`_builtins.string`) and enums are not in `Index::types`.
    Named(String),
    Array(Box<TypeRef>),
    /// Dictionary with user-defined keys.
    Dict(Box<TypeRef>),
    Union(Vec<TypeRef>),
    Bool,
    /// A name of a field in the target index (spec `_types.Field`).
    Field,
    /// Dictionary keyed by field names (spec `dictionary_of` with a `_types.Field` key).
    FieldDict(Box<TypeRef>),
    /// A spec `literal_value`, stored as its JSON text (`"x"` keeps its quotes, `true` does not).
    Literal(String),
    /// User-defined values and builtins with nothing to complete.
    Other,
}
