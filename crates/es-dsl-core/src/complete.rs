//! Key completion: request block -> endpoint -> cursor path -> descend the type table.

use std::sync::Arc;

use crate::candidate::describe;
use crate::request::METHODS;
use crate::schema::visible;
use crate::{
    Candidate, Cursor, Endpoint, Field, FieldKind, Fields, Index, Kind, NoFields, Prop, Seg, Shape,
    Slot, TypeRef, Version, block_at, cursor_at,
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
    let env = Env {
        index,
        version,
        fields,
        scope: match nested_scope(body, &path) {
            Some(path) => FieldScope::Nested(path),
            None => FieldScope::Outer,
        },
    };
    let prefix = match slot {
        Slot::Key { prefix } => prefix,
        Slot::Value | Slot::Str { .. } => {
            let target = target_index(endpoint, &block.path);
            return complete_value(
                &env,
                &root,
                &path,
                &slot,
                target,
                index_name_slot(endpoint, &path),
            );
        }
    };

    let mut names: Vec<Candidate>;
    let mut wants_fields = false;
    let mut policies = None;
    if path.is_empty() {
        names = root_children(index, &root, version)
            .into_iter()
            .map(|p| key(index, p))
            .collect();
    } else {
        let tys = resolve(index, &root, &path, version);
        if tys.is_empty() {
            return Err(NoCompletion::UnknownPath);
        }
        wants_fields = tys.iter().any(|t| field_keyed(index, t));
        // The field-keyed container says which mapping types make sense (range versus prefix, ...).
        if wants_fields {
            policies = field_policies(index, &tys);
        }
        names = tys
            .into_iter()
            .flat_map(|t| children(index, t, version))
            .map(|p| key(index, p))
            .collect();
    }
    let mut no_mapping = false;
    if wants_fields {
        let found = fields.fields(target_index(endpoint, &block.path));
        no_mapping = found.is_empty();
        names.extend(
            found
                .iter()
                .filter(|f| field_allowed(policies.as_deref(), f, &env.scope))
                .map(|f| Candidate::new(f.name.clone(), Kind::Field, Some(f.ty.clone()))),
        );
    }
    names.retain(|n| match_rank(&prefix, &n.label).is_some());
    sort_ranked(&mut names, &prefix);
    if names.is_empty() {
        return Err(if no_mapping {
            NoCompletion::NoMapping
        } else {
            NoCompletion::NoMatch
        });
    }
    Ok(names)
}

fn key(index: &Index, prop: &Prop) -> Candidate {
    Candidate::new(prop.name.as_str(), Kind::Key, describe(&prop.ty))
        .shaped(shape_of(index, &prop.ty))
        .documented(prop)
}

/// What container a declared key's value takes, when the schema has no scalar alternative.
fn shape_of(index: &Index, ty: &TypeRef) -> Option<Shape> {
    match classify(index, ty) {
        Some(ShapeClass::Object) => Some(Shape::Object),
        Some(ShapeClass::Array) => Some(Shape::Array),
        Some(ShapeClass::Scalar) | None => None,
    }
}

enum ShapeClass {
    Object,
    Array,
    Scalar,
}

/// A union with a scalar form is left alone: inserting a container would pick one form for the user.
fn classify(index: &Index, ty: &TypeRef) -> Option<ShapeClass> {
    match ty {
        TypeRef::Named(n) if index.types.contains_key(n) => Some(ShapeClass::Object),
        TypeRef::Dict(_) | TypeRef::FieldDict(_) => Some(ShapeClass::Object),
        TypeRef::Array(_) => Some(ShapeClass::Array),
        TypeRef::Union(ts) => {
            let mut classes = ts.iter().filter_map(|t| classify(index, t));
            let mut out = classes.next()?;
            for class in classes {
                out = match (out, class) {
                    (ShapeClass::Scalar, _) | (_, ShapeClass::Scalar) => ShapeClass::Scalar,
                    (ShapeClass::Object, _) | (_, ShapeClass::Object) => ShapeClass::Object,
                    (ShapeClass::Array, ShapeClass::Array) => ShapeClass::Array,
                };
            }
            Some(out)
        }
        _ => Some(ShapeClass::Scalar),
    }
}

/// Which mapping fields a field-keyed clause accepts. Unknown containers produce no policy, so
/// their field list stays unfiltered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldPolicy {
    /// `term`, `terms`: exact values.
    Exact,
    /// `match` and its phrase variants: analyzed text, but also numerics.
    Analyzed,
    /// `prefix`, `wildcard`, `regexp`, `fuzzy`: string fields only.
    String,
    /// `range`: the PRD narrows this to numeric and date fields.
    Range,
    /// `geo_*`: spatial fields only.
    Geo,
    /// The field key under `sort`.
    Sortable,
    /// The `field` key of an aggregation: anything an aggregation can bucket or measure.
    Aggregatable,
    /// The `path` key of a nested query: the enclosing nested field only.
    NestedPath,
}

impl FieldPolicy {
    fn allows(self, field: &Field) -> bool {
        let kind = field.kind();
        match self {
            Self::Exact | Self::Analyzed => {
                !matches!(kind, FieldKind::Object | FieldKind::Nested | FieldKind::Geo)
            }
            Self::String => matches!(kind, FieldKind::Text | FieldKind::Keyword),
            Self::Range => matches!(kind, FieldKind::Numeric | FieldKind::Date),
            Self::Geo => matches!(kind, FieldKind::Geo),
            Self::Sortable | Self::Aggregatable => matches!(
                kind,
                FieldKind::Keyword
                    | FieldKind::Numeric
                    | FieldKind::Date
                    | FieldKind::Boolean
                    | FieldKind::Ip
                    | FieldKind::Version
            ),
            Self::NestedPath => kind == FieldKind::Nested,
        }
    }
}

/// Which mapping fields the cursor's context can address.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FieldScope {
    /// Outside a nested clause: nested children are unreachable.
    Outer,
    /// Inside a `nested` clause that names this path: only that nested document's fields.
    Nested(String),
    /// Source filtering (`_source.includes`/`excludes`) can name any field path.
    Source,
}

impl FieldScope {
    fn allows(&self, field: &Field) -> bool {
        match self {
            Self::Outer => !field.nested,
            Self::Nested(path) => field
                .name
                .strip_prefix(path)
                .is_some_and(|rest| rest.starts_with('.')),
            Self::Source => true,
        }
    }
}

/// Whether the cursor's context can address `field`.
fn field_allowed(policies: Option<&[FieldPolicy]>, field: &Field, scope: &FieldScope) -> bool {
    scope.allows(field) && policies.is_none_or(|ps| ps.iter().any(|p| p.allows(field)))
}

/// The nested path the cursor sits in, from the `path` of the enclosing `nested` clause. Queries,
/// nested sorts and nested aggregations all use `{"nested": {"path": "<field>"}}`, and only a path
/// written before the cursor is visible in the prefix being completed.
fn nested_scope(body: &str, path: &[Seg]) -> Option<String> {
    let paths = crate::json_path::string_values(body, "path");
    path.iter().enumerate().rev().find_map(|(i, seg)| {
        if !matches!(seg, Seg::Key(k) if k == "nested") {
            return None;
        }
        paths
            .iter()
            .find(|(parent, _)| parent.as_slice() == &path[..=i])
            .map(|(_, value)| value.clone())
    })
}

/// The field whose value the cursor is in: the last path key under a field-keyed type, provided
/// only array indices follow it (`terms: {"field": [(...)]}`).
fn value_field_name<'a>(env: &Env<'_>, root: &Root, path: &'a [Seg]) -> Option<&'a str> {
    let (at, name) = path
        .iter()
        .enumerate()
        .rev()
        .find_map(|(at, seg)| match seg {
            Seg::Key(k) => Some((at, k.as_str())),
            Seg::Index(_) => None,
        })?;
    if !path[at + 1..].iter().all(|s| matches!(s, Seg::Index(_))) {
        return None;
    }
    let parents = resolve(env.index, root, &path[..at], env.version);
    parents
        .iter()
        .any(|t| field_keyed(env.index, t))
        .then_some(name)
}

/// Whether the spec models `ty` as taking a boolean inline value. The spec writes the inline form
/// `<field>: <scalar>` as a clause object with a `value`/`query` scalar property, so the literal is
/// read from the schema's own shape rather than a list of clause names.
fn takes_inline_bool(index: &Index, ty: &TypeRef) -> bool {
    match ty {
        TypeRef::Bool => true,
        TypeRef::Array(t) => takes_inline_bool(index, t),
        TypeRef::Union(ts) => ts.iter().any(|t| takes_inline_bool(index, t)),
        TypeRef::Named(n) => index.types.get(n).is_some_and(|props| {
            props
                .iter()
                .any(|p| matches!(p.name.as_str(), "value" | "query") && bool_scalar(&p.ty))
        }),
        TypeRef::Dict(_) | TypeRef::FieldDict(_) | TypeRef::Field | TypeRef::Literal(_) => false,
        TypeRef::Other => false,
    }
}

/// Whether `ty` is a boolean or a union/array that includes one.
fn bool_scalar(ty: &TypeRef) -> bool {
    match ty {
        TypeRef::Bool => true,
        TypeRef::Array(t) => bool_scalar(t),
        TypeRef::Union(ts) => ts.iter().any(bool_scalar),
        _ => false,
    }
}

/// Policies for a value position that takes field names (`sort: ["..."]`, `aggs.x.terms.field`,
/// `nested.path`). The resolved value type covers the first case; the parent container covers the
/// two property slots, which are plain `Field` references with no type information of their own.
fn value_field_policies(
    index: &Index,
    root: &Root,
    path: &[Seg],
    tys: &[&TypeRef],
    version: Option<Version>,
) -> Option<Vec<FieldPolicy>> {
    let mut out = field_policies(index, tys).unwrap_or_default();
    let Some(Seg::Key(key)) = path.last() else {
        return (!out.is_empty()).then_some(out);
    };
    if !matches!(key.as_str(), "field" | "path") {
        return (!out.is_empty()).then_some(out);
    }
    let parents = resolve(index, root, &path[..path.len() - 1], version);
    for ty in parents {
        let TypeRef::Named(name) = ty else {
            continue;
        };
        let short = name.rsplit('.').next().unwrap_or(name);
        match (key.as_str(), short) {
            ("field", n) if n.ends_with("Aggregation") => {
                out.push(FieldPolicy::Aggregatable);
            }
            ("path", "NestedQuery") => out.push(FieldPolicy::NestedPath),
            _ => {}
        }
    }
    out.sort_by_key(|p| *p as u8);
    out.dedup();
    (!out.is_empty()).then_some(out)
}

/// Policies for the types at a field-key position, from the container names the spec uses.
fn field_policies(index: &Index, tys: &[&TypeRef]) -> Option<Vec<FieldPolicy>> {
    let mut out = Vec::new();
    for ty in tys {
        collect_field_policies(index, ty, &mut out);
    }
    out.sort_by_key(|p| *p as u8);
    out.dedup();
    (!out.is_empty()).then_some(out)
}

fn collect_field_policies(index: &Index, ty: &TypeRef, out: &mut Vec<FieldPolicy>) {
    match ty {
        TypeRef::FieldDict(value) => collect_value_policies(value, out),
        TypeRef::Named(n) => {
            if index.open.get(n).is_some_and(|o| o.is_field)
                && let Some(policy) = policy_of_container(n)
            {
                out.push(policy);
            }
        }
        TypeRef::Array(t) => collect_field_policies(index, t, out),
        TypeRef::Union(ts) => ts
            .iter()
            .for_each(|t| collect_field_policies(index, t, out)),
        _ => {}
    }
}

fn collect_value_policies(ty: &TypeRef, out: &mut Vec<FieldPolicy>) {
    match ty {
        TypeRef::Named(n) => {
            if let Some(policy) = policy_of_container(n) {
                out.push(policy);
            }
        }
        TypeRef::Array(t) => collect_value_policies(t, out),
        TypeRef::Union(ts) => ts.iter().for_each(|t| collect_value_policies(t, out)),
        _ => {}
    }
}

fn policy_of_container(name: &str) -> Option<FieldPolicy> {
    let short = name.rsplit('.').next().unwrap_or(name);
    Some(match short {
        "TermQuery" | "TermsQuery" | "SpanTermQuery" | "TermsSetQuery" => FieldPolicy::Exact,
        "MatchQuery"
        | "MatchPhraseQuery"
        | "MatchPhrasePrefixQuery"
        | "MatchBoolPrefixQuery"
        | "CommonTermsQuery" => FieldPolicy::Analyzed,
        "PrefixQuery" | "WildcardQuery" | "RegexpQuery" | "FuzzyQuery" | "IntervalsQuery" => {
            FieldPolicy::String
        }
        // Every range variant, including the generic one and the keyword (`TermRangeQuery`) form:
        // the PRD asks for numeric and date fields only.
        "DateRangeQuery"
        | "NumberRangeQuery"
        | "LongNumberRangeQuery"
        | "TermRangeQuery"
        | "UntypedRangeQuery"
        | "RangeQuery" => FieldPolicy::Range,
        "GeoDistanceQuery"
        | "GeoBoundingBoxQuery"
        | "GeoPolygonQuery"
        | "GeoShapeQuery"
        | "GeoGridQuery"
        | "GeoDistanceSort"
        | "ShapeQuery" => FieldPolicy::Geo,
        "SortOptions" | "FieldSort" => FieldPolicy::Sortable,
        _ => return None,
    })
}

/// Sorts by label; when two candidates share one (a key and a field name), the first in schema order wins.
fn sort_dedup(cs: &mut Vec<Candidate>) {
    cs.sort_by(|a, b| a.label.cmp(&b.label));
    cs.dedup_by(|a, b| a.label == b.label);
}

/// How well `label` answers the typed `prefix`: exact, then prefix, then case-insensitive prefix,
/// then a scattered subsequence. The `usize` is the number of characters the subsequence match
/// skips, so the tightest fuzzy hit sorts first (`stok` prefers `stock` over `in_stock`).
/// `None` when the label does not match at all.
fn match_rank(prefix: &str, label: &str) -> Option<(u8, usize)> {
    if prefix.is_empty() {
        return Some((0, 0));
    }
    if label == prefix {
        Some((0, 0))
    } else if label.starts_with(prefix) {
        Some((1, 0))
    } else if starts_with_ignore_case(label, prefix) {
        Some((2, 0))
    } else {
        subsequence_skip(prefix, label).map(|skip| (3, skip))
    }
}

fn starts_with_ignore_case(label: &str, prefix: &str) -> bool {
    let mut chars = label.chars();
    prefix
        .chars()
        .all(|p| chars.next().is_some_and(|l| l.eq_ignore_ascii_case(&p)))
}

/// Characters skipped by the leftmost case-insensitive subsequence match; `None` when `prefix` is
/// not a subsequence of `label`. Leading and internal skips both count.
fn subsequence_skip(prefix: &str, label: &str) -> Option<usize> {
    let mut wanted = prefix.chars();
    let mut next = wanted.next()?;
    let (mut first, mut last, mut matched) = (None, 0, 0);
    for (at, ch) in label.chars().enumerate() {
        if ch.eq_ignore_ascii_case(&next) {
            first.get_or_insert(at);
            last = at;
            matched += 1;
            match wanted.next() {
                Some(n) => next = n,
                None => break,
            }
        }
    }
    if matched != prefix.chars().count() {
        return None;
    }
    let first = first?;
    Some(first + (last - first + 1 - matched))
}

/// Orders matches by rank, then alphabetically, and drops duplicate labels. With no prefix the
/// order stays alphabetical, so an untouched list reads predictably.
fn sort_ranked(cs: &mut Vec<Candidate>, prefix: &str) {
    if prefix.is_empty() {
        sort_dedup(cs);
        return;
    }
    // Decorate once: the comparator would otherwise rescan labels on every swap.
    let mut keyed: Vec<((u8, usize), usize, Candidate)> = cs
        .drain(..)
        .map(|c| {
            let rank = match_rank(prefix, &c.label).unwrap_or((u8::MAX, usize::MAX));
            (rank, c.label.chars().count(), c)
        })
        .collect();
    keyed.sort_by(|a, b| {
        a.0.cmp(&b.0)
            // Among fuzzy hits the shorter label is the closer guess; prefix ties stay alphabetical.
            .then_with(|| {
                if a.0.0 == 3 {
                    a.1.cmp(&b.1)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| a.2.label.cmp(&b.2.label))
    });
    keyed.dedup_by(|a, b| a.2.label == b.2.label);
    cs.extend(keyed.into_iter().map(|(_, _, c)| c));
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

fn root_children<'a>(index: &'a Index, root: &'a Root, version: Option<Version>) -> Vec<&'a Prop> {
    match root {
        Root::Props(ps) => ps.iter().filter(|p| visible(p.since, version)).collect(),
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

/// Whether the cursor's value slot names an index: the `_index` of a bulk action line or the
/// `index` of an `_msearch` header. The spec types both as plain strings, so the endpoint is the
/// only place that says what the string means.
fn index_name_slot(endpoint: &Endpoint, path: &[Seg]) -> bool {
    let ndjson = endpoint.urls.iter().any(|u| {
        ["/_bulk", "/_msearch", "/_msearch/template"]
            .iter()
            .any(|suffix| u.path.ends_with(suffix))
    });
    ndjson && matches!(path.last(), Some(Seg::Key(k)) if k == "_index" || k == "index")
}

/// The schema, the cluster version, the target's mapping and the field scope the cursor is in:
/// everything completion reads.
struct Env<'a> {
    index: &'a Index,
    version: Option<Version>,
    fields: &'a dyn Fields,
    scope: FieldScope,
}

/// Value completion: enum members, booleans and literals. Outside a string, string values come back quoted.
fn complete_value(
    env: &Env<'_>,
    root: &Root,
    path: &[Seg],
    slot: &Slot,
    target: &str,
    index_slot: bool,
) -> Result<Vec<Candidate>, NoCompletion> {
    if path.is_empty() {
        return Err(NoCompletion::ValueSlot);
    }
    let tys = resolve(env.index, root, path, env.version);
    if tys.is_empty() {
        return Err(NoCompletion::UnknownPath);
    }
    let wants_fields = tys.iter().any(|t| takes_field(t));
    // The value may sit under a mapping field (`term: {"field": ...}`), which needs the mapping even
    // though the value itself is not a field name.
    let value_field = value_field_name(env, root, path);
    let found = if wants_fields || value_field.is_some() {
        env.fields.fields(target)
    } else {
        Arc::from([])
    };
    let no_mapping = wants_fields && found.is_empty();
    let policies = if wants_fields {
        value_field_policies(env.index, root, path, &tys, env.version)
    } else {
        None
    };
    // Source filtering can name nested children, which no query clause outside `nested` can reach.
    let source = path
        .iter()
        .any(|s| matches!(s, Seg::Key(k) if k == "_source"));
    let source_scope = FieldScope::Source;
    let scope = if source { &source_scope } else { &env.scope };
    let prefix = match slot {
        Slot::Str { prefix } => prefix.as_str(),
        Slot::Key { .. } | Slot::Value => "",
    };
    let index_names = if index_slot {
        env.fields.names()
    } else {
        Vec::new()
    };
    // A boolean mapping field takes `true`/`false` as its inline value. The spec writes that inline
    // form as a clause object with a `value`/`query` scalar property, so the mapping type and the
    // schema shape together decide whether the literal applies.
    let bool_values: Vec<&str> = if !matches!(slot, Slot::Str { .. })
        && tys.iter().any(|t| takes_inline_bool(env.index, t))
        && value_field
            .and_then(|name| found.iter().find(|f| f.name == name))
            .is_some_and(|f| f.kind() == FieldKind::Boolean)
    {
        vec!["true", "false"]
    } else {
        Vec::new()
    };
    let mut vals: Vec<Candidate> = tys
        .into_iter()
        .flat_map(|t| values(env.index, t))
        .map(|(text, is_str)| (text, is_str, Kind::Value, None))
        .chain(
            // Only a slot that *is* a field name lists the mapping's fields; a field's value does
            // not, even though the mapping had to be loaded to read its type.
            found
                .iter()
                .filter(|_| wants_fields)
                .filter(|f| field_allowed(policies.as_deref(), f, scope))
                .map(|f| (f.name.clone(), true, Kind::FieldValue, Some(f.ty.clone()))),
        )
        .chain(
            index_names
                .into_iter()
                .map(|n| (n, true, Kind::IndexValue, None)),
        )
        .chain(
            bool_values
                .into_iter()
                .map(|t| (t.to_string(), false, Kind::Value, None)),
        )
        .filter_map(|(text, is_str, kind, detail)| {
            let label = match slot {
                Slot::Str { .. } => is_str
                    .then_some(text)
                    .filter(|t| match_rank(prefix, t).is_some()),
                _ if is_str => Some(format!("\"{text}\"")),
                _ => Some(text),
            }?;
            Some(Candidate::new(label, kind, detail))
        })
        .collect();
    sort_ranked(&mut vals, prefix);
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
    // A comma list completes its last element only.
    let word = partial.rsplit(',').next().unwrap_or_default();
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
        .filter(|next| !next.starts_with('{') && match_rank(partial, next).is_some())
        .map(|next| Candidate::new(next, Kind::Path, None))
        .collect();
    if urls().any(|tpl| tpl[segs.len()] == "{index}") {
        found.extend(
            fields
                .names()
                .into_iter()
                .filter(|n| match_rank(word, n).is_some())
                .map(|n| Candidate::new(n, Kind::Index, None)),
        );
    }
    sort_ranked(&mut found, word);
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
        .filter(|p| {
            visible(p.since, version) && match_rank(partial, &p.name).is_some() && !used(&p.name)
        })
        .map(|p| Candidate::new(p.name.as_str(), Kind::Param, describe(&p.ty)))
        .collect();
    sort_ranked(&mut names, partial);
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

fn children<'a>(index: &'a Index, ty: &'a TypeRef, version: Option<Version>) -> Vec<&'a Prop> {
    match ty {
        TypeRef::Named(n) => index.types.get(n).map_or_else(Vec::new, |ps| {
            ps.iter().filter(|p| visible(p.since, version)).collect()
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
            fn fields(&self, target: &str) -> Arc<[Field]> {
                if target != "logs" {
                    return Arc::from([]);
                }
                Arc::from(["title", "title.keyword", "user"].map(|name| Field {
                    name: name.into(),
                    ty: "keyword".into(),
                    nested: false,
                }))
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
    fn field_candidates_follow_the_clause() {
        use crate::OpenKey;
        use NoCompletion::*;

        struct Logs;
        impl Fields for Logs {
            fn fields(&self, target: &str) -> Arc<[Field]> {
                if target != "logs" {
                    return Arc::from([]);
                }
                let plain = |name: &str, ty: &str| Field {
                    name: name.into(),
                    ty: ty.into(),
                    nested: false,
                };
                let nested = |name: &str, ty: &str| Field {
                    name: name.into(),
                    ty: ty.into(),
                    nested: true,
                };
                vec![
                    plain("title", "text"),
                    plain("title.keyword", "keyword"),
                    plain("price", "double"),
                    plain("@timestamp", "date"),
                    plain("user", "object"),
                    plain("user.name", "keyword"),
                    plain("loc", "geo_point"),
                    plain("busy", "boolean"),
                    plain("addr", "ip"),
                    plain("notes", "flattened"),
                    plain("user_alias", "alias"),
                    plain("relation", "join"),
                    plain("tags", "nested"),
                    nested("tags.name", "keyword"),
                    plain("stock", "integer"),
                    plain("in_stock", "boolean"),
                ]
                .into()
            }
        }

        let sort = || {
            TypeRef::Union(vec![
                TypeRef::Field,
                named("SortOptions"),
                TypeRef::Array(Box::new(TypeRef::Union(vec![
                    TypeRef::Field,
                    named("SortOptions"),
                ]))),
            ])
        };
        let mut idx = Index {
            endpoints: vec![Endpoint {
                name: "search".into(),
                urls: vec![Url {
                    path: "/{index}/_search".into(),
                    methods: vec!["POST".into()],
                }],
                body: vec![
                    prop("term", TypeRef::FieldDict(Box::new(named("TermQuery")))),
                    prop("terms", named("TermsQuery")),
                    prop("match", TypeRef::FieldDict(Box::new(named("MatchQuery")))),
                    prop(
                        "range",
                        TypeRef::FieldDict(Box::new(TypeRef::Union(vec![
                            named("DateRangeQuery"),
                            named("NumberRangeQuery"),
                            named("TermRangeQuery"),
                            named("UntypedRangeQuery"),
                        ]))),
                    ),
                    prop("prefix", TypeRef::FieldDict(Box::new(named("PrefixQuery")))),
                    prop("geo_distance", named("GeoDistanceQuery")),
                    prop(
                        "custom",
                        TypeRef::FieldDict(Box::new(named("UnknownKeyedQuery"))),
                    ),
                    prop("sort", sort()),
                    prop(
                        "aggs",
                        TypeRef::Dict(Box::new(named("AggregationContainer"))),
                    ),
                    prop("nested", named("NestedQuery")),
                    prop("multi_match", named("MultiMatchQuery")),
                    prop("_source", named("SourceFilter")),
                ],
                ..Endpoint::default()
            }],
            ..Index::default()
        };
        let scalar = || {
            TypeRef::Union(vec![
                TypeRef::Other,
                named("_builtins.number"),
                TypeRef::Bool,
            ])
        };
        // The spec writes the inline `<field>: <scalar>` form as a clause object with a scalar body.
        idx.types.insert(
            "TermQuery".into(),
            vec![prop("value", scalar()), prop("boost", TypeRef::Other)],
        );
        idx.types.insert(
            "MatchQuery".into(),
            vec![prop("query", scalar()), prop("boost", TypeRef::Other)],
        );
        idx.types
            .insert("PrefixQuery".into(), vec![prop("value", TypeRef::Other)]);
        idx.types.insert(
            "SourceFilter".into(),
            vec![
                prop("includes", TypeRef::Array(Box::new(TypeRef::Field))),
                prop("excludes", TypeRef::Array(Box::new(TypeRef::Field))),
            ],
        );
        for range in [
            "DateRangeQuery",
            "NumberRangeQuery",
            "TermRangeQuery",
            "UntypedRangeQuery",
        ] {
            idx.types.insert(range.into(), Vec::new());
        }
        idx.types.insert("TermsQuery".into(), Vec::new());
        idx.open.insert(
            "TermsQuery".into(),
            OpenKey {
                is_field: true,
                value: TypeRef::Other,
            },
        );
        idx.types.insert("GeoDistanceQuery".into(), Vec::new());
        idx.open.insert(
            "GeoDistanceQuery".into(),
            OpenKey {
                is_field: true,
                value: TypeRef::Other,
            },
        );
        idx.types.insert("SortOptions".into(), Vec::new());
        idx.open.insert(
            "SortOptions".into(),
            OpenKey {
                is_field: true,
                value: TypeRef::Other,
            },
        );
        idx.types.insert(
            "AggregationContainer".into(),
            vec![prop("terms", named("TermsAggregation"))],
        );
        idx.types.insert(
            "TermsAggregation".into(),
            vec![prop("field", TypeRef::Field)],
        );
        idx.types.insert(
            "NestedQuery".into(),
            vec![
                prop("path", TypeRef::Field),
                prop("query", named("NestedBody")),
            ],
        );
        idx.types.insert(
            "NestedBody".into(),
            vec![prop(
                "match",
                TypeRef::FieldDict(Box::new(named("MatchQuery"))),
            )],
        );
        idx.types.insert(
            "MultiMatchQuery".into(),
            vec![prop("fields", TypeRef::Array(Box::new(TypeRef::Field)))],
        );

        let at = |text: &str| {
            let cursor = text.find('|').unwrap();
            let text = text.replacen('|', "", 1);
            complete_with(&idx, &text, cursor, None, &Logs).unwrap_or_default()
        };
        let case = |input: &str, has: &[&str], lacks: &[&str]| {
            let got = at(input);
            for want in has {
                assert!(got.iter().any(|g| g == want), "{input}: missing {want}");
            }
            for unwanted in lacks {
                assert!(
                    !got.iter().any(|g| g == unwanted),
                    "{input}: unexpected {unwanted}"
                );
            }
        };

        // Exact and analyzed clauses keep text, keyword, numeric, date, boolean and ip fields.
        // Flattened, alias and join fields are queryable by name, so they stay too.
        for clause in ["term", "terms", "match"] {
            case(
                &format!("POST logs/_search\n{{\"{clause}\": {{\"|\n"),
                &[
                    "title",
                    "title.keyword",
                    "price",
                    "@timestamp",
                    "user.name",
                    "busy",
                    "addr",
                    "notes",
                    "user_alias",
                    "relation",
                ],
                &["user", "loc", "tags.name"],
            );
        }
        // PRD A4: range only offers numeric and date fields.
        case(
            "POST logs/_search\n{\"range\": {\"|\n",
            &["price", "@timestamp"],
            &["title", "title.keyword", "user.name", "busy", "addr", "loc"],
        );
        // String clauses only offer fields prefix/wildcard/regexp can search.
        case(
            "POST logs/_search\n{\"prefix\": {\"|\n",
            &["title", "title.keyword", "user.name", "notes"],
            &[
                "price",
                "@timestamp",
                "busy",
                "addr",
                "user",
                "loc",
                "user_alias",
                "relation",
            ],
        );
        // Geo clauses only offer spatial fields.
        case(
            "POST logs/_search\n{\"geo_distance\": {\"|\n",
            &["loc"],
            &["title", "price", "@timestamp", "user.name", "busy", "addr"],
        );
        // Sorting excludes text without a keyword sub-field, objects, nested and geo fields.
        case(
            "POST logs/_search\n{\"sort\": [{\"|\n",
            &[
                "title.keyword",
                "price",
                "@timestamp",
                "user.name",
                "busy",
                "addr",
                "notes",
            ],
            &[
                "title",
                "user",
                "loc",
                "tags.name",
                "tags",
                "user_alias",
                "relation",
            ],
        );
        // A clause the policy table does not know still hides nested children, which no plain
        // clause can reach.
        case(
            "POST logs/_search\n{\"custom\": {\"|\n",
            &["title", "user"],
            &["tags.name"],
        );
        // A field array (here `sort`) gets the same policy as the keyed form.
        case(
            "POST logs/_search\n{\"sort\": [\"|\n",
            &[
                "title.keyword",
                "price",
                "@timestamp",
                "user.name",
                "busy",
                "addr",
                "notes",
            ],
            &[
                "title",
                "user",
                "loc",
                "tags.name",
                "tags",
                "user_alias",
                "relation",
            ],
        );
        // An aggregation's `field` takes aggregatable fields, not text or objects.
        case(
            "POST logs/_search\n{\"aggs\": {\"by_user\": {\"terms\": {\"field\": \"|\n",
            &[
                "title.keyword",
                "price",
                "@timestamp",
                "user.name",
                "busy",
                "addr",
                "notes",
            ],
            &[
                "title",
                "user",
                "loc",
                "tags.name",
                "user_alias",
                "relation",
            ],
        );
        // `nested.path` names the nested field itself, not its children.
        case(
            "POST logs/_search\n{\"nested\": {\"path\": \"|\n",
            &["tags"],
            &["tags.name", "title", "user", "price"],
        );
        // Inside the nested query, only that nested document's fields are addressable; outside it
        // they stay hidden.
        case(
            "POST logs/_search\n{\"nested\": {\"path\": \"tags\", \"query\": {\"match\": {\"|\n",
            &["tags.name"],
            &["title", "user", "tags", "price", "stock"],
        );
        // Source filtering names nested children, which no query clause outside `nested` reaches.
        case(
            "POST logs/_search\n{\"_source\": {\"includes\": [\"|\n",
            &["tags.name", "title", "user.name"],
            &[],
        );
        // Field arrays that are only `Field` values still hide nested children.
        case(
            "POST logs/_search\n{\"multi_match\": {\"fields\": [\"|\n",
            &["title", "user", "loc"],
            &["tags.name"],
        );
        // A target with no mapping still reports NoMapping rather than an empty list.
        let cursor = "POST other/_search\n{\"term\": {\"|\n".find('|').unwrap();
        let text = "POST other/_search\n{\"term\": {\"|\n".replacen('|', "", 1);
        assert_eq!(
            complete_with(&idx, &text, cursor, None, &Logs),
            Err(NoMapping)
        );

        // Fields used as values (an array element, an aggregation's `field`) are not keys: the
        // client must insert them without the `: ` that a field key gets.
        for text in [
            "POST logs/_search\n{\"sort\": [\"",
            "POST logs/_search\n{\"aggs\": {\"by_user\": {\"terms\": {\"field\": \"",
            "POST logs/_search\n{\"nested\": {\"path\": \"",
        ] {
            let cs = candidates_with(&idx, text, text.len(), None, &Logs).unwrap();
            assert!(
                cs.iter().all(|c| c.kind == Kind::FieldValue),
                "{text}: {:?}",
                cs.iter()
                    .map(|c| (c.label.as_str(), c.kind))
                    .collect::<Vec<_>>()
            );
        }

        // Ranked matching: an exact label beats its prefixed extensions, and with no prefix hit a
        // scattered case-insensitive subsequence still finds `@timestamp` for `ts`.
        let labels = |input: &str| {
            let cursor = input.find('|').unwrap();
            let text = input.replacen('|', "", 1);
            complete_with(&idx, &text, cursor, None, &Logs).unwrap_or_default()
        };
        assert_eq!(
            labels("POST logs/_search\n{\"term\": {\"title|\""),
            ["title", "title.keyword"]
        );
        assert_eq!(
            labels("POST logs/_search\n{\"term\": {\"TITLE|\""),
            ["title", "title.keyword"]
        );
        assert!(
            labels("POST logs/_search\n{\"term\": {\"ts|\"").contains(&"@timestamp".to_string())
        );
        // Among fuzzy hits the tighter match ranks first, so a typo does not surface a longer
        // field that merely contains the same letters.
        assert_eq!(
            labels("POST logs/_search\n{\"term\": {\"stok|\""),
            ["stock", "in_stock"]
        );
        // A boolean mapping field takes its literals in the inline clause form, but not inside a
        // string (the user is typing a term) and not where the clause has its own value (sort).
        assert_eq!(
            labels("POST logs/_search\n{\"term\": {\"busy\": |"),
            ["false", "true"]
        );
        assert_eq!(
            labels("POST logs/_search\n{\"match\": {\"busy\": |"),
            ["false", "true"]
        );
        assert!(labels("POST logs/_search\n{\"term\": {\"title\": |").is_empty());
        assert!(labels("POST logs/_search\n{\"term\": {\"busy\": \"|").is_empty());
        assert!(labels("POST logs/_search\n{\"sort\": [{\"busy\": |").is_empty());
    }

    #[test]
    fn keys_carry_the_container_shape_but_fields_do_not() {
        use crate::Shape;

        struct NoFieldsAtAll;
        impl Fields for NoFieldsAtAll {
            fn fields(&self, _: &str) -> Arc<[Field]> {
                Arc::from([])
            }
        }

        let q = named("Q");
        let mut idx = Index {
            endpoints: vec![Endpoint {
                name: "search".into(),
                urls: vec![Url {
                    path: "/{index}/_search".into(),
                    methods: vec!["POST".into()],
                }],
                body: vec![
                    prop("query", q.clone()),
                    prop("aggs", TypeRef::Dict(Box::new(named("Agg")))),
                    prop("fields", TypeRef::Array(Box::new(named("FieldAndFormat")))),
                    prop(
                        "sort",
                        TypeRef::Union(vec![
                            TypeRef::Field,
                            named("SortOptions"),
                            TypeRef::Array(Box::new(named("SortOptions"))),
                        ]),
                    ),
                    prop(
                        "_source",
                        TypeRef::Union(vec![TypeRef::Bool, named("SourceFilter")]),
                    ),
                    prop("size", TypeRef::Other),
                    prop(
                        "either",
                        TypeRef::Union(vec![named("Q"), TypeRef::Array(Box::new(named("Q")))]),
                    ),
                ],
                ..Endpoint::default()
            }],
            ..Index::default()
        };
        idx.types
            .insert("Q".into(), vec![prop("bool", named("Bool"))]);
        idx.types
            .insert("Bool".into(), vec![prop("must", TypeRef::Other)]);
        idx.types
            .insert("Agg".into(), vec![prop("terms", TypeRef::Other)]);
        idx.types.insert("FieldAndFormat".into(), Vec::new());
        idx.types.insert("SourceFilter".into(), Vec::new());
        idx.types.insert("SortOptions".into(), Vec::new());

        let at = |text: &str| {
            let cursor = text.find('|').unwrap();
            let text = text.replacen('|', "", 1);
            candidates_with(&idx, &text, cursor, None, &NoFieldsAtAll).unwrap()
        };
        let shape = |cs: &[Candidate], label: &str| {
            cs.iter()
                .find(|c| c.label == label)
                .unwrap_or_else(|| panic!("no {label}"))
                .shape
        };

        let root = at("POST logs/_search\n{\"|");
        assert_eq!(shape(&root, "query"), Some(Shape::Object));
        assert_eq!(shape(&root, "aggs"), Some(Shape::Object));
        assert_eq!(shape(&root, "fields"), Some(Shape::Array));
        // A scalar alternative exists, so no container is forced.
        assert_eq!(shape(&root, "sort"), None);
        assert_eq!(shape(&root, "_source"), None);
        assert_eq!(shape(&root, "size"), None);
        // Both forms of the union are containers; a single object is the better default.
        assert_eq!(shape(&root, "either"), Some(Shape::Object));

        let query = at("POST logs/_search\n{\"query\": {\"|");
        assert_eq!(shape(&query, "bool"), Some(Shape::Object));
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
            shape: None,
            doc: None,
            default: None,
            deprecated: false,
        };
        let shaped = |label: &str, kind, detail: Option<&str>, shape| Candidate {
            shape: Some(shape),
            ..c(label, kind, detail)
        };
        assert_eq!(
            at("GET idx/_search\n{\"q|"),
            [shaped("query", Kind::Key, Some("Q"), Shape::Object)]
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
        idx.types.get_mut("Op").unwrap()[0] = prop("index", named("IndexAction"));
        idx.types.insert(
            "IndexAction".into(),
            vec![
                prop("_index", named("_builtins.string")),
                prop("_id", named("_builtins.string")),
            ],
        );
        idx.types.get_mut("Head").unwrap()[0] = prop("index", named("_builtins.string"));
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

        // The index name is a plain string in the spec; `_bulk` and `_msearch` still know it.
        struct Names;
        impl Fields for Names {
            fn fields(&self, _: &str) -> Arc<[Field]> {
                Arc::from([])
            }
            fn names(&self) -> Vec<String> {
                ["logs".into(), "metrics".into()].to_vec()
            }
        }
        for input in [
            "POST _bulk\n{\"index\": {\"_index\": \"|",
            "POST _msearch\n{\"index\": \"|",
        ] {
            let cursor = input.find('|').unwrap();
            let text = input.replacen('|', "", 1);
            assert_eq!(
                complete_with(&idx, &text, cursor, None, &Names),
                ok(&["logs", "metrics"]),
                "{input}"
            );
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
            fn fields(&self, _: &str) -> Arc<[crate::Field]> {
                Arc::from([])
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
