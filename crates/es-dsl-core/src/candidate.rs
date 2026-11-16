//! A completion candidate: what to insert plus enough context for a client to render it.

use crate::TypeRef;

/// What a candidate stands for, so clients can pick an icon and sort groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Method,
    /// An index, data stream or alias name in the `{index}` URL segment.
    Index,
    /// A literal URL path segment.
    Path,
    /// A query-string parameter name.
    Param,
    /// A JSON object key declared by the schema.
    Key,
    /// A JSON value: enum member, boolean or literal.
    Value,
    /// A field of the target index, from its mapping.
    Field,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Text to insert; for values outside a string it includes the quotes.
    pub label: String,
    pub kind: Kind,
    /// Short type description: a schema type for keys and parameters, the mapping type for fields.
    pub detail: Option<String>,
}

impl Candidate {
    pub(crate) fn new(label: impl Into<String>, kind: Kind, detail: Option<String>) -> Self {
        Self {
            label: label.into(),
            kind,
            detail,
        }
    }
}

/// One-line description of a schema type, e.g. `boolean`, `Query[]`, `{field: Query}`; `None` when there is nothing useful to say.
pub(crate) fn describe(ty: &TypeRef) -> Option<String> {
    match ty {
        TypeRef::Named(n) => Some(n.rsplit('.').next().unwrap_or(n).to_string()),
        TypeRef::Array(t) => describe(t).map(|d| format!("{d}[]")),
        TypeRef::Dict(t) => describe(t).map(|d| format!("{{name: {d}}}")),
        TypeRef::FieldDict(t) => Some(format!(
            "{{field: {}}}",
            describe(t).unwrap_or_else(|| "any".into())
        )),
        TypeRef::Union(ts) => {
            let mut parts: Vec<String> = ts.iter().filter_map(describe).collect();
            parts.dedup();
            (!parts.is_empty()).then(|| parts.join(" | "))
        }
        TypeRef::Bool => Some("boolean".into()),
        TypeRef::Field => Some("field".into()),
        TypeRef::Literal(l) => Some(l.clone()),
        TypeRef::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_types() {
        let named = |n: &str| TypeRef::Named(n.into());
        let cases = [
            (named("_types.query_dsl.Query"), Some("Query")),
            (TypeRef::Array(Box::new(named("a.B"))), Some("B[]")),
            (
                TypeRef::Dict(Box::new(TypeRef::Bool)),
                Some("{name: boolean}"),
            ),
            (
                TypeRef::FieldDict(Box::new(TypeRef::Other)),
                Some("{field: any}"),
            ),
            (
                TypeRef::Union(vec![TypeRef::Other, named("a.B"), TypeRef::Bool]),
                Some("B | boolean"),
            ),
            (TypeRef::Union(vec![TypeRef::Other]), None),
            (TypeRef::Other, None),
        ];
        for (ty, want) in cases {
            assert_eq!(describe(&ty).as_deref(), want, "{ty:?}");
        }
    }
}
