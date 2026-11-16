//! Key completion: request block -> endpoint -> cursor path -> descend the type table.

use crate::candidate::describe;
use crate::request::METHODS;
use crate::schema::visible;
use crate::{
    Candidate, Cursor, Endpoint, Fields, Index, Kind, NoFields, Prop, Seg, Slot, TypeRef, Version,
    block_at, cursor_at,
};

/// Why no candidates were produced, so callers never see a silent empty result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoCompletion {
    NoRequest,
    UnknownEndpoint,
    /// The cursor is on an NDJSON line that holds a user document (bulk source line), which has no schema.
    FreeForm,
    /// The cursor is in a value position with no known property to take values from (e.g. top level).
    ValueSlot,
    /// The path leaves the indexed types (enum, alias, `inherits`, user-defined value).
    UnknownPath,
    /// The position takes field names but no mapping is available for the target index.
    NoMapping,
    NoMatch,
}

/// Returns the candidates at `cursor` (byte offset into `text`), sorted and deduplicated:
/// methods and path segments on a request line, body keys elsewhere. With `version` set, endpoints and
/// properties introduced in a later stack version are left out.
pub fn complete(
    index: &Index,
    text: &str,
    cursor: usize,
    version: Option<Version>,
) -> Result<Vec<String>, NoCompletion> {
    complete_with(index, text, cursor, version, &NoFields)
}

/// Like [`complete`], and also offers field names from `fields` where the schema expects one
/// (`exists.field`, `match.<field>`, `aggs.x.terms.field`, ...). Labels only; see [`candidates_with`].
pub fn complete_with(
    index: &Index,
    text: &str,
    cursor: usize,
    version: Option<Version>,
    fields: &dyn Fields,
) -> Result<Vec<String>, NoCompletion> {
    candidates_with(index, text, cursor, version, fields)
        .map(|cs| cs.into_iter().map(|c| c.label).collect())
}

/// Like [`complete_with`], but each candidate carries its kind and a type description.
pub fn candidates_with(
    index: &Index,
    text: &str,
    cursor: usize,
    version: Option<Version>,
    fields: &dyn Fields,
) -> Result<Vec<Candidate>, NoCompletion> {
    let line_start = text[..cursor].rfind('\n').map_or(0, |i| i + 1);
    if let Some(r) = complete_request_line(
        index,
        text[line_start..cursor].trim_start(),
        version,
        fields,
    ) {
        return r;
    }
    let block = block_at(text, cursor).ok_or(NoCompletion::NoRequest)?;
    let endpoint = match_endpoint(index, &block.method, &block.path, version)
        .ok_or(NoCompletion::UnknownEndpoint)?;
    let start = block.body.start.min(cursor);
    let (root, body) = body_root(endpoint, &text[start..cursor])?;
    let Cursor { path, slot } = cursor_at(body);
    let prefix = match slot {
        Slot::Key { prefix } => prefix,
        Slot::Value | Slot::Str { .. } => {
            let target = target_index(endpoint, &block.path);
            return complete_value(index, &root, &path, &slot, version, fields, target);
        }
    };

    let mut names: Vec<Candidate>;
    let mut wants_fields = false;
    if path.is_empty() {
        names = root_children(index, &root, version)
            .into_iter()
            .map(key)
            .collect();
    } else {
        let tys = resolve(index, &root, &path, version);
        if tys.is_empty() {
            return Err(NoCompletion::UnknownPath);
        }
        wants_fields = tys.iter().any(|t| field_keyed(index, t));
        names = tys
            .into_iter()
            .flat_map(|t| children(index, t, version))
            .map(key)
            .collect();
    }
    let mut no_mapping = false;
    if wants_fields {
        let found = fields.fields(target_index(endpoint, &block.path));
        no_mapping = found.is_empty();
        names.extend(
            found
                .into_iter()
                .map(|f| Candidate::new(f.name, Kind::Field, Some(f.ty))),
        );
    }
    names.retain(|n| n.label.starts_with(&prefix));
    sort_dedup(&mut names);
    if names.is_empty() {
        return Err(if no_mapping {
            NoCompletion::NoMapping
        } else {
            NoCompletion::NoMatch
        });
    }
    Ok(names)
}

fn key((name, ty): (&str, &TypeRef)) -> Candidate {
    Candidate::new(name, Kind::Key, describe(ty))
}

/// Sorts by label; when two candidates share one (a key and a field name), the first in schema order wins.
fn sort_dedup(cs: &mut Vec<Candidate>) {
    cs.sort_by(|a, b| a.label.cmp(&b.label));
    cs.dedup_by(|a, b| a.label == b.label);
}

/// The path segment that stands for `{index}` in the endpoint's matching URL template, or `""`.
fn target_index<'a>(endpoint: &Endpoint, path: &'a str) -> &'a str {
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    endpoint
        .urls
        .iter()
        .find_map(|u| {
            let tpl: Vec<&str> = u.path.trim_matches('/').split('/').collect();
            let i = tpl.iter().position(|t| *t == "{index}")?;
            (tpl.len() == segs.len()
                && tpl
                    .iter()
                    .zip(&segs)
                    .all(|(t, s)| t.starts_with('{') || t == s))
            .then(|| segs[i])
        })
        .unwrap_or("")
}

/// Whether keys under an object of type `ty` are field names.
fn field_keyed(index: &Index, ty: &TypeRef) -> bool {
    match ty {
        TypeRef::FieldDict(_) => true,
        TypeRef::Named(n) => index.open.get(n).is_some_and(|o| o.is_field),
        TypeRef::Array(t) => field_keyed(index, t),
        TypeRef::Union(ts) => ts.iter().any(|t| field_keyed(index, t)),
        _ => false,
    }
}

/// Whether a value of type `ty` can be a field name.
fn takes_field(ty: &TypeRef) -> bool {
    match ty {
        TypeRef::Field => true,
        TypeRef::Array(t) => takes_field(t),
        TypeRef::Union(ts) => ts.iter().any(takes_field),
        _ => false,
    }
}

/// Where the cursor's JSON document starts: the endpoint's property set, or the types its root value can take.
pub(crate) enum Root<'a> {
    Props(&'a [Prop]),
    Types(Vec<&'a TypeRef>),
}

/// Picks the root for the body text before the cursor. For NDJSON bodies this selects the current line
/// and the type that line is expected to have; the returned text is what the JSON scanner should see.
pub(crate) fn body_root<'a, 'b>(
    endpoint: &'a Endpoint,
    body: &'b str,
) -> Result<(Root<'a>, &'b str), NoCompletion> {
    if endpoint.lines.is_empty() {
        let root = match &endpoint.body_ty {
            Some(t) if endpoint.body.is_empty() => Root::Types(vec![t]),
            _ => Root::Props(&endpoint.body),
        };
        return Ok((root, body));
    }
    let (prior, current) = body.rsplit_once('\n').unwrap_or(("", body));
    let items = &endpoint.lines;
    // Lines alternate action/payload. An action that has no payload (bulk `delete`) is followed by another action.
    let mut pending: Option<&str> = None;
    for line in prior.lines().filter(|l| !l.trim().is_empty()) {
        pending = match pending {
            Some(_) => None,
            None => {
                let key = first_key(line);
                (items.len() < 3 || key != "delete").then_some(key)
            }
        };
    }
    let ty = match (items.len(), pending) {
        (1, _) | (_, None) => &items[0],
        (2, Some(_)) => &items[1],
        (_, Some("update")) => &items[1],
        _ => return Err(NoCompletion::FreeForm),
    };
    Ok((Root::Types(vec![ty]), current))
}

/// The first object key on a line, or `""`; only used to tell bulk actions apart.
fn first_key(line: &str) -> &str {
    line.trim_start()
        .strip_prefix('{')
        .map(str::trim_start)
        .and_then(|l| l.strip_prefix('"'))
        .and_then(|l| l.split_once('"'))
        .map_or("", |(k, _)| k)
}

fn root_children<'a>(
    index: &'a Index,
    root: &'a Root,
    version: Option<Version>,
) -> Vec<(&'a str, &'a TypeRef)> {
    match root {
        Root::Props(ps) => ps
            .iter()
            .filter(|p| visible(p.since, version))
            .map(|p| (p.name.as_str(), &p.ty))
            .collect(),
        Root::Types(ts) => ts
            .iter()
            .flat_map(|t| children(index, t, version))
            .collect(),
    }
}

/// Follows `path` from the body root down to the types the cursor's slot can hold.
pub(crate) fn resolve<'a>(
    index: &'a Index,
    root: &Root<'a>,
    path: &[Seg],
    version: Option<Version>,
) -> Vec<&'a TypeRef> {
    let (mut tys, rest) = match root {
        Root::Props(ps) => match path.split_first() {
            Some((first, rest)) => (step_props(ps, first, version), rest),
            None => return Vec::new(),
        },
        Root::Types(ts) => (ts.clone(), path),
    };
    for seg in rest {
        tys = tys
            .into_iter()
            .flat_map(|t| step(index, t, seg, version))
            .collect();
    }
    tys
}

/// Value completion: enum members, booleans and literals. Outside a string, string values come back quoted.
fn complete_value(
    index: &Index,
    root: &Root,
    path: &[Seg],
    slot: &Slot,
    version: Option<Version>,
    fields: &dyn Fields,
    target: &str,
) -> Result<Vec<Candidate>, NoCompletion> {
    if path.is_empty() {
        return Err(NoCompletion::ValueSlot);
    }
    let tys = resolve(index, root, path, version);
    if tys.is_empty() {
        return Err(NoCompletion::UnknownPath);
    }
    let wants_fields = tys.iter().any(|t| takes_field(t));
    let found = if wants_fields {
        fields.fields(target)
    } else {
        Vec::new()
    };
    let no_mapping = wants_fields && found.is_empty();
    let mut vals: Vec<Candidate> = tys
        .into_iter()
        .flat_map(|t| values(index, t))
        .map(|(text, is_str)| (text, is_str, Kind::Value, None))
        .chain(
            found
                .into_iter()
                .map(|f| (f.name, true, Kind::Field, Some(f.ty))),
        )
        .filter_map(|(text, is_str, kind, detail)| {
            let label = match slot {
                Slot::Str { prefix } => is_str.then_some(text).filter(|t| t.starts_with(prefix)),
                _ if is_str => Some(format!("\"{text}\"")),
                _ => Some(text),
            }?;
            Some(Candidate::new(label, kind, detail))
        })
        .collect();
    sort_dedup(&mut vals);
    if vals.is_empty() {
        return Err(if no_mapping {
            NoCompletion::NoMapping
        } else {
            NoCompletion::NoMatch
        });
    }
    Ok(vals)
}

/// Candidate values of a type as `(text, is_string)`; `text` has no quotes.
fn values(index: &Index, ty: &TypeRef) -> Vec<(String, bool)> {
    match ty {
        TypeRef::Named(n) => index
            .enums
            .get(n)
            .map_or_else(Vec::new, |m| m.iter().map(|v| (v.clone(), true)).collect()),
        TypeRef::Bool => vec![("true".into(), false), ("false".into(), false)],
        TypeRef::Literal(l) => match l.strip_prefix('"').and_then(|l| l.strip_suffix('"')) {
            Some(s) => vec![(s.to_string(), true)],
            None => vec![(l.clone(), false)],
        },
        TypeRef::Array(t) => values(index, t),
        TypeRef::Union(ts) => ts.iter().flat_map(|t| values(index, t)).collect(),
        TypeRef::Dict(_) | TypeRef::FieldDict(_) | TypeRef::Field | TypeRef::Other => Vec::new(),
    }
}

/// `prefix` is the request line up to the cursor. `None` means this is not a request line (so the
/// cursor is in a body); a first word that is no method prefix counts as body text too.
fn complete_request_line(
    index: &Index,
    prefix: &str,
    version: Option<Version>,
    fields: &dyn Fields,
) -> Option<Result<Vec<Candidate>, NoCompletion>> {
    let Some((method, rest)) = prefix.split_once(char::is_whitespace) else {
        let up = prefix.to_ascii_uppercase();
        let found: Vec<Candidate> = METHODS
            .iter()
            .filter(|m| !up.is_empty() && m.starts_with(&up))
            .map(|m| Candidate::new(*m, Kind::Method, None))
            .collect();
        return (!found.is_empty()).then_some(Ok(found));
    };
    let method = METHODS.iter().find(|m| m.eq_ignore_ascii_case(method))?;
    if let Some((path, query)) = rest.trim_start().split_once('?') {
        return Some(complete_query(index, method, path, query, version));
    }
    let mut segs: Vec<&str> = rest
        .trim_start()
        .trim_start_matches('/')
        .split('/')
        .collect();
    let partial = segs.pop().unwrap_or_default();
    let urls = || {
        index
            .endpoints
            .iter()
            .filter(|e| visible(e.since, version))
            .flat_map(|e| &e.urls)
            .filter(|u| u.methods.iter().any(|m| m == method))
            .map(|u| u.path.trim_matches('/').split('/').collect::<Vec<_>>())
            .filter(|tpl| {
                let typed_match = tpl
                    .iter()
                    .zip(&segs)
                    .all(|(t, s)| t.starts_with('{') || t == s);
                typed_match && tpl.len() > segs.len()
            })
    };
    let mut found: Vec<Candidate> = urls()
        .map(|tpl| tpl[segs.len()])
        .filter(|next| !next.starts_with('{') && next.starts_with(partial))
        .map(|next| Candidate::new(next, Kind::Path, None))
        .collect();
    if urls().any(|tpl| tpl[segs.len()] == "{index}") {
        // A comma list completes its last element only.
        let word = partial.rsplit(',').next().unwrap_or_default();
        found.extend(
            fields
                .names()
                .into_iter()
                .filter(|n| n.starts_with(word))
                .map(|n| Candidate::new(n, Kind::Index, None)),
        );
    }
    sort_dedup(&mut found);
    Some(if found.is_empty() {
        Err(NoCompletion::NoMatch)
    } else {
        Ok(found)
    })
}

/// Completes a query-string parameter name; `query` is the text after `?` up to the cursor.
fn complete_query(
    index: &Index,
    method: &str,
    path: &str,
    query: &str,
    version: Option<Version>,
) -> Result<Vec<Candidate>, NoCompletion> {
    let endpoint =
        match_endpoint(index, method, path, version).ok_or(NoCompletion::UnknownEndpoint)?;
    let (done, partial) = query.rsplit_once('&').unwrap_or(("", query));
    if partial.contains('=') {
        return Err(NoCompletion::ValueSlot);
    }
    let used = |name: &str| done.split('&').any(|kv| kv.split('=').next() == Some(name));
    let mut names: Vec<Candidate> = endpoint
        .query
        .iter()
        .filter(|p| visible(p.since, version) && p.name.starts_with(partial) && !used(&p.name))
        .map(|p| Candidate::new(p.name.as_str(), Kind::Param, describe(&p.ty)))
        .collect();
    sort_dedup(&mut names);
    if names.is_empty() {
        return Err(NoCompletion::NoMatch);
    }
    Ok(names)
}

/// Picks the endpoint whose URL template matches; the template with most literal segments wins
/// (`/{index}/_search` over `/{index}/{id}`).
pub(crate) fn match_endpoint<'a>(
    index: &'a Index,
    method: &str,
    path: &str,
    version: Option<Version>,
) -> Option<&'a Endpoint> {
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    index
        .endpoints
        .iter()
        .filter(|e| visible(e.since, version))
        .flat_map(|e| e.urls.iter().map(move |u| (e, u)))
        .filter(|(_, u)| u.methods.iter().any(|m| m == method))
        .filter_map(|(e, u)| {
            let tpl: Vec<&str> = u.path.trim_matches('/').split('/').collect();
            if tpl.len() != segs.len() {
                return None;
            }
            let mut literals = 0;
            for (t, s) in tpl.iter().zip(&segs) {
                if t.starts_with('{') {
                    if s.is_empty() {
                        return None;
                    }
                } else if t == s {
                    literals += 1;
                } else {
                    return None;
                }
            }
            Some((literals, e))
        })
        .max_by_key(|(literals, _)| *literals)
        .map(|(_, e)| e)
}

fn step_props<'a>(props: &'a [Prop], seg: &Seg, version: Option<Version>) -> Vec<&'a TypeRef> {
    match seg {
        Seg::Key(k) => props
            .iter()
            .filter(|p| p.name == *k && visible(p.since, version))
            .map(|p| &p.ty)
            .collect(),
        Seg::Index(_) => Vec::new(),
    }
}

/// Moves one path segment down from `ty`. A key on an array type steps into the element,
/// because the spec lets most array-valued fields take a single object.
fn step<'a>(
    index: &'a Index,
    ty: &'a TypeRef,
    seg: &Seg,
    version: Option<Version>,
) -> Vec<&'a TypeRef> {
    match (ty, seg) {
        (TypeRef::Named(n), Seg::Key(_)) => {
            let declared = index
                .types
                .get(n)
                .map_or_else(Vec::new, |props| step_props(props, seg, version));
            // A key that is not declared goes to the type's open key, if it has one.
            match (declared.is_empty(), index.open.get(n)) {
                (true, Some(open)) => vec![&open.value],
                _ => declared,
            }
        }
        (TypeRef::Array(t), Seg::Index(_)) => vec![t],
        (TypeRef::Array(t), Seg::Key(_)) => step(index, t, seg, version),
        (TypeRef::Dict(t) | TypeRef::FieldDict(t), Seg::Key(_)) => vec![t],
        (TypeRef::Union(ts), _) => ts
            .iter()
            .flat_map(|t| step(index, t, seg, version))
            .collect(),
        _ => Vec::new(),
    }
}

fn children<'a>(
    index: &'a Index,
    ty: &'a TypeRef,
    version: Option<Version>,
) -> Vec<(&'a str, &'a TypeRef)> {
    match ty {
        TypeRef::Named(n) => index.types.get(n).map_or_else(Vec::new, |ps| {
            ps.iter()
                .filter(|p| visible(p.since, version))
                .map(|p| (p.name.as_str(), &p.ty))
                .collect()
        }),
        TypeRef::Array(t) => children(index, t, version),
        TypeRef::Union(ts) => ts
            .iter()
            .flat_map(|t| children(index, t, version))
            .collect(),
        TypeRef::Dict(_)
        | TypeRef::FieldDict(_)
        | TypeRef::Field
        | TypeRef::Bool
        | TypeRef::Literal(_)
        | TypeRef::Other => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Url;

    fn named(n: &str) -> TypeRef {
        TypeRef::Named(n.into())
    }

    fn prop(name: &str, ty: TypeRef) -> Prop {
        Prop::new(name, ty)
    }

    fn index() -> Index {
        let ep = |name: &str, path: &str, methods: &[&str], body: Vec<Prop>| Endpoint {
            query: ["pretty", "preference", "q"]
                .map(|n| prop(n, TypeRef::Other))
                .to_vec(),
            name: name.into(),
            urls: vec![Url {
                path: path.into(),
                methods: methods.iter().map(|m| m.to_string()).collect(),
            }],
            body,
            since: None,
            ..Endpoint::default()
        };
        let mut idx = Index {
            endpoints: vec![
                ep(
                    "search",
                    "/{index}/_search",
                    &["GET", "POST"],
                    vec![
                        prop("query", named("Q")),
                        prop("size", TypeRef::Other),
                        prop("order", named("Order")),
                        prop("explain", TypeRef::Bool),
                        prop("open", named("Open")),
                        prop("mode", TypeRef::Literal("\"fast\"".into())),
                    ],
                ),
                ep(
                    "get",
                    "/{index}/{id}",
                    &["GET"],
                    vec![prop("nope", TypeRef::Other)],
                ),
            ],
            ..Index::default()
        };
        idx.types
            .insert("Open".into(), vec![prop("fixed", TypeRef::Bool)]);
        idx.open.insert(
            "Open".into(),
            crate::OpenKey {
                is_field: true,
                value: named("Order"),
            },
        );
        idx.enums
            .insert("Order".into(), vec!["asc".into(), "desc".into()]);
        idx.types.insert(
            "Q".into(),
            vec![
                prop("bool", named("Bool")),
                prop("match", TypeRef::Dict(Box::new(named("M")))),
            ],
        );
        idx.types.insert(
            "Bool".into(),
            vec![prop(
                "must",
                TypeRef::Union(vec![named("Q"), TypeRef::Array(Box::new(named("Q")))]),
            )],
        );
        idx.types
            .insert("M".into(), vec![prop("query", TypeRef::Other)]);
        idx
    }

    #[test]
    fn table() {
        use NoCompletion::*;
        let ok = |v: &[&str]| Ok(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let cases: [(&str, Result<Vec<String>, NoCompletion>); 19] = [
            (
                "GET idx/_search\n{|",
                ok(&["explain", "mode", "open", "order", "query", "size"]),
            ),
            ("GET idx/_search\n{\"q|", ok(&["query"])),
            ("GET idx/_search\n{\"query\": {|", ok(&["bool", "match"])),
            (
                "GET idx/_search\n{\"query\": {\"bool\": {\"must\": {|",
                ok(&["bool", "match"]),
            ),
            (
                "GET idx/_search\n{\"query\": {\"bool\": {\"must\": [{\"b|",
                ok(&["bool"]),
            ),
            (
                "GET idx/_search\n{\"query\": {\"match\": {\"title\": {|",
                ok(&["query"]),
            ),
            ("GET idx/_search\n{\"query\": {\"match\": {|", Err(NoMatch)),
            (
                "GET idx/_search\n{\"query\": {\"zzz\": {|",
                Err(UnknownPath),
            ),
            ("GET idx/_search\n{\"query\": |", Err(NoMatch)),
            (
                "GET idx/_search\n{\"order\": |",
                ok(&["\"asc\"", "\"desc\""]),
            ),
            ("GET idx/_search\n{\"order\": \"d|", ok(&["desc"])),
            ("GET idx/_search\n{\"explain\": |", ok(&["false", "true"])),
            ("GET idx/_search\n{\"explain\": \"|", Err(NoMatch)),
            ("GET idx/_search\n{\"mode\": \"|", ok(&["fast"])),
            ("GET idx/_search\n{\"zzz\": |", Err(UnknownPath)),
            // undeclared keys fall through to the open key's value type; declared ones win
            (
                "GET idx/_search\n{\"open\": {\"any_field\": |",
                ok(&["\"asc\"", "\"desc\""]),
            ),
            (
                "GET idx/_search\n{\"open\": {\"fixed\": |",
                ok(&["false", "true"]),
            ),
            ("POST idx/_nothing\n{|", Err(UnknownEndpoint)),
            ("{|", Err(NoRequest)),
        ];
        for (input, want) in cases {
            let cursor = input.find('|').unwrap();
            let text = input.replacen('|', "", 1);
            assert_eq!(complete(&index(), &text, cursor, None), want, "{input}");
        }
    }

    #[test]
    fn field_names_come_from_the_target_index_mapping() {
        use crate::{Field, OpenKey};
        use NoCompletion::*;
        struct Logs;
        impl Fields for Logs {
            fn fields(&self, target: &str) -> Vec<Field> {
                if target != "logs" {
                    return Vec::new();
                }
                ["title", "title.keyword", "user"]
                    .map(|name| Field {
                        name: name.into(),
                        ty: "keyword".into(),
                    })
                    .to_vec()
            }
        }
        let mut idx = Index {
            endpoints: vec![Endpoint {
                name: "search".into(),
                urls: vec![Url {
                    path: "/{index}/_search".into(),
                    methods: vec!["POST".into()],
                }],
                body: vec![
                    prop("exists", named("Ex")),
                    prop("by_field", TypeRef::FieldDict(Box::new(TypeRef::Bool))),
                    prop("open", named("Open")),
                    prop("many", TypeRef::Array(Box::new(TypeRef::Field))),
                ],
                ..Endpoint::default()
            }],
            ..Index::default()
        };
        idx.types
            .insert("Ex".into(), vec![prop("field", TypeRef::Field)]);
        idx.types
            .insert("Open".into(), vec![prop("fixed", TypeRef::Bool)]);
        idx.open.insert(
            "Open".into(),
            OpenKey {
                is_field: true,
                value: TypeRef::Bool,
            },
        );
        let ok = |v: &[&str]| Ok(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let cases: [(&str, Result<Vec<String>, NoCompletion>); 11] = [
            (
                "POST logs/_search\n{\"by_field\": {|",
                ok(&["title", "title.keyword", "user"]),
            ),
            (
                "POST logs/_search\n{\"by_field\": {\"title.|",
                ok(&["title.keyword"]),
            ),
            // declared keys and field names are offered together
            (
                "POST logs/_search\n{\"open\": {|",
                ok(&["fixed", "title", "title.keyword", "user"]),
            ),
            // the value under a field key still resolves through the dictionary
            (
                "POST logs/_search\n{\"by_field\": {\"title\": |",
                ok(&["false", "true"]),
            ),
            (
                "POST logs/_search\n{\"exists\": {\"field\": |",
                ok(&["\"title\"", "\"title.keyword\"", "\"user\""]),
            ),
            (
                "POST logs/_search\n{\"exists\": {\"field\": \"ti|",
                ok(&["title", "title.keyword"]),
            ),
            ("POST logs/_search\n{\"many\": [\"u|", ok(&["user"])),
            // no mapping for this index: say so instead of an empty list
            ("POST other/_search\n{\"by_field\": {|", Err(NoMapping)),
            (
                "POST other/_search\n{\"exists\": {\"field\": |",
                Err(NoMapping),
            ),
            // a prefix that matches nothing is NoMatch, not NoMapping
            ("POST logs/_search\n{\"by_field\": {\"zz|", Err(NoMatch)),
            ("POST logs/_search\n{\"exists\": {|", ok(&["field"])),
        ];
        for (input, want) in cases {
            let cursor = input.find('|').unwrap();
            let text = input.replacen('|', "", 1);
            assert_eq!(
                complete_with(&idx, &text, cursor, None, &Logs),
                want,
                "{input}"
            );
        }
    }

    #[test]
    fn candidates_carry_kind_and_type_detail() {
        let at = |text: &str| {
            let cursor = text.find('|').unwrap();
            candidates_with(
                &index(),
                &text.replacen('|', "", 1),
                cursor,
                None,
                &NoFields,
            )
            .unwrap()
        };
        let c = |label: &str, kind, detail: Option<&str>| Candidate {
            label: label.into(),
            kind,
            detail: detail.map(String::from),
        };
        assert_eq!(
            at("GET idx/_search\n{\"q|"),
            [c("query", Kind::Key, Some("Q"))]
        );
        assert_eq!(
            at("GET idx/_search\n{\"explain\": |"),
            [c("false", Kind::Value, None), c("true", Kind::Value, None)]
        );
        assert_eq!(at("PO|"), [c("POST", Kind::Method, None)]);
        assert_eq!(at("GET /idx/_se|"), [c("_search", Kind::Path, None)]);
        assert_eq!(
            at("GET idx/_search?pre|"),
            [
                c("preference", Kind::Param, None),
                c("pretty", Kind::Param, None)
            ]
        );
    }

    #[test]
    fn ndjson_and_value_bodies() {
        use NoCompletion::*;
        let ep = |path: &str, lines: Vec<TypeRef>, body_ty: Option<TypeRef>| Endpoint {
            name: path.into(),
            urls: vec![Url {
                path: path.into(),
                methods: vec!["POST".into()],
            }],
            lines,
            body_ty,
            ..Endpoint::default()
        };
        let mut idx = Index {
            endpoints: vec![
                ep(
                    "/_bulk",
                    vec![named("Op"), named("Upd"), TypeRef::Other],
                    None,
                ),
                ep("/_msearch", vec![named("Head"), named("Body")], None),
                ep("/_settings", vec![], Some(named("Settings"))),
            ],
            ..Index::default()
        };
        for (name, keys) in [
            ("Op", &["index", "create", "update", "delete"][..]),
            ("Upd", &["doc", "script"]),
            ("Head", &["index", "preference"]),
            ("Body", &["query", "size"]),
            ("Settings", &["refresh_interval"]),
        ] {
            idx.types.insert(
                name.into(),
                keys.iter().map(|k| prop(k, TypeRef::Other)).collect(),
            );
        }
        let ok = |v: &[&str]| Ok(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let cases: [(&str, Result<Vec<String>, NoCompletion>); 11] = [
            // bulk: action, then source (free-form), then action again
            (
                "POST _bulk\n{|",
                ok(&["create", "delete", "index", "update"]),
            ),
            ("POST _bulk\n{\"index\":{}}\n{|", Err(FreeForm)),
            (
                "POST _bulk\n{\"index\":{}}\n{\"a\":1}\n{\"cr|",
                ok(&["create"]),
            ),
            // update's payload is the update action, not a document
            ("POST _bulk\n{\"update\":{}}\n{\"d|", ok(&["doc"])),
            // delete has no payload line
            (
                "POST _bulk\n{\"delete\":{}}\n{|",
                ok(&["create", "delete", "index", "update"]),
            ),
            (
                "POST _bulk\n\n{\"index\":{}}\n\n{\"a\":1}\n{|",
                ok(&["create", "delete", "index", "update"]),
            ),
            // msearch: header, body, header, ...
            ("POST _msearch\n{|", ok(&["index", "preference"])),
            ("POST _msearch\n{}\n{\"q|", ok(&["query"])),
            ("POST _msearch\n{}\n{}\n{\"pr|", ok(&["preference"])),
            // a body that is one value type completes from that type
            ("POST _settings\n{\"r|", ok(&["refresh_interval"])),
            ("POST _settings\n{\"zz|", Err(NoMatch)),
        ];
        for (input, want) in cases {
            let cursor = input.find('|').unwrap();
            let text = input.replacen('|', "", 1);
            assert_eq!(complete(&idx, &text, cursor, None), want, "{input}");
        }
    }

    #[test]
    fn request_line_table() {
        use NoCompletion::*;
        let ok = |v: &[&str]| Ok(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let cases: [(&str, Result<Vec<String>, NoCompletion>); 12] = [
            ("P|", ok(&["POST", "PUT"])),
            ("  de|", ok(&["DELETE"])),
            // the first segment of every template is a placeholder, so there is nothing literal to offer
            ("POST |", Err(NoMatch)),
            ("POST idx/_se|", ok(&["_search"])),
            ("POST /idx/|", ok(&["_search"])),
            ("PUT idx/|", Err(NoMatch)),
            ("POST idx/_search?pre|", ok(&["preference", "pretty"])),
            ("POST idx/_search?pretty&|", ok(&["preference", "q"])),
            ("POST idx/_search?pretty&pretty|", Err(NoMatch)),
            ("POST idx/_search?q=a|", Err(ValueSlot)),
            ("POST idx/_nope?|", Err(UnknownEndpoint)),
            ("POST idx/_search\n{\"q|", ok(&["query"])),
        ];
        for (input, want) in cases {
            let cursor = input.find('|').unwrap();
            let text = input.replacen('|', "", 1);
            assert_eq!(complete(&index(), &text, cursor, None), want, "{input}");
        }
    }

    #[test]
    fn hides_what_the_cluster_does_not_have_yet() {
        let mut idx = index();
        idx.endpoints[0].body[1].since = Some([8, 0, 0]); // `size`
        idx.types.get_mut("Q").unwrap()[0].since = Some([7, 5, 0]); // `bool`
        idx.endpoints[1].since = Some([9, 0, 0]);
        let at = |text: &str, v| {
            let cursor = text.find('|').unwrap();
            complete(&idx, &text.replacen('|', "", 1), cursor, v)
        };
        let ok = |v: &[&str]| Ok(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let body = "GET idx/_search\n{|";
        let base = ["explain", "mode", "open", "order", "query"];
        let with_size = ["explain", "mode", "open", "order", "query", "size"];
        assert_eq!(at(body, Some([7, 17, 0])), ok(&base));
        assert_eq!(at(body, Some([8, 0, 0])), ok(&with_size));
        assert_eq!(at(body, None), ok(&with_size));
        let q = "GET idx/_search\n{\"query\": {|";
        assert_eq!(at(q, Some([7, 4, 9])), ok(&["match"]));
        assert_eq!(at(q, Some([7, 5, 0])), ok(&["bool", "match"]));
        // the `GET /{index}/{id}` endpoint is 9.0+, so its body keys are gone on 8.x
        let get = "GET idx/1\n{|";
        assert_eq!(
            at(get, Some([8, 19, 0])),
            Err(NoCompletion::UnknownEndpoint)
        );
        assert_eq!(at(get, Some([9, 0, 0])), ok(&["nope"]));
    }

    #[test]
    fn parses_versions() {
        use crate::parse_version;
        assert_eq!(parse_version("8.11"), Some([8, 11, 0]));
        assert_eq!(parse_version("8.19.24"), Some([8, 19, 24]));
        assert_eq!(parse_version("9"), Some([9, 0, 0]));
        assert_eq!(parse_version("8.x"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
    }

    #[test]
    fn literal_template_beats_placeholder() {
        let idx = index();
        assert_eq!(
            match_endpoint(&idx, "GET", "/a/_search", None)
                .unwrap()
                .name,
            "search"
        );
        assert_eq!(
            match_endpoint(&idx, "GET", "/a/1", None).unwrap().name,
            "get"
        );
    }

    #[test]
    fn index_names_complete_in_the_index_segment() {
        struct Names;
        impl Fields for Names {
            fn fields(&self, _: &str) -> Vec<crate::Field> {
                Vec::new()
            }
            fn names(&self) -> Vec<String> {
                ["logs-1", "logs-2", "users"].map(String::from).into()
            }
        }
        let cases = [
            ("GET lo|", vec!["logs-1", "logs-2"]),
            ("GET logs-1,u|", vec!["users"]),
            ("GET users/_se|", vec!["_search"]),
            ("GET /us|", vec!["users"]),
        ];
        for (text, want) in cases {
            let cursor = text.find('|').unwrap();
            let text = text.replace('|', "");
            let got = candidates_with(&index(), &text, cursor, None, &Names)
                .unwrap()
                .into_iter()
                .map(|c| c.label)
                .collect::<Vec<_>>();
            assert_eq!(got, want, "{text}");
        }
    }

    #[test]
    fn never_panics_on_any_prefix() {
        let text = "GET idx/_search\n{\"query\": {\"bool\": {\"must\": [{\"match\": {\"a\": {\"query\": \"é\"}}}]}}}";
        for (i, _) in text.char_indices() {
            let _ = complete(&index(), text, i, None);
        }
    }

    proptest::proptest! {
        /// Random request-shaped soup with the cursor anywhere: must never panic.
        #[test]
        fn never_panics_on_random_text(
            parts in proptest::collection::vec(
                proptest::sample::select(vec![
                    "GET ", "POST ", "idx/_search", "_bulk", "?q=", "\n", "{", "}", "[", "]",
                    "\"query\"", "\"bool\"", "\"must\"", "\"a\"", ":", ",", "\"", "\\", "é", "😀", " ",
                ]),
                0..40,
            ),
            at in 0usize..400,
        ) {
            let text = parts.concat();
            let mut cursor = at.min(text.len());
            while !text.is_char_boundary(cursor) {
                cursor -= 1;
            }
            let _ = candidates_with(&index(), &text, cursor, None, &NoFields);
        }
    }
}
