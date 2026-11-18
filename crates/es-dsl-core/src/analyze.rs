//! Hover and diagnostics: the same request -> endpoint -> body-root -> type-table walk as completion,
//! applied to the keys that are already in the text.
//!
//! Diagnostics only report what the schema can prove. A key under a type the index cannot see into
//! (enum, user-defined value, unresolved path) is never flagged.

use std::ops::Range;

use crate::candidate::describe;
use crate::complete::{Root, body_root, match_endpoint, resolve};
use crate::request::{Block, split_blocks};
use crate::schema::visible;
use crate::{Endpoint, Index, Prop, Seg, Token, TypeRef, Version, keys};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// A problem in the text; `range` is a non-empty byte range into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range<usize>,
    pub severity: Severity,
    pub message: String,
}

/// Markdown documentation for the token under the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hover {
    pub range: Range<usize>,
    pub markdown: String,
}

/// What an object type says about one key.
#[derive(Default)]
struct Lookup<'a> {
    props: Vec<&'a Prop>,
    /// The key is accepted without being declared (`AdditionalProperty`, dictionaries).
    open: bool,
    /// Some type on the way cannot be inspected, so absence proves nothing.
    opaque: bool,
}

impl<'a> Lookup<'a> {
    fn add(&mut self, index: &'a Index, ty: &'a TypeRef, key: &str) {
        match ty {
            TypeRef::Named(n) => match index.types.get(n) {
                Some(ps) => {
                    self.props.extend(ps.iter().filter(|p| p.name == key));
                    self.open |= index.open.contains_key(n);
                }
                None => self.opaque = true,
            },
            TypeRef::Array(t) => self.add(index, t, key),
            TypeRef::Union(ts) => ts.iter().for_each(|t| self.add(index, t, key)),
            TypeRef::Dict(_) | TypeRef::FieldDict(_) => self.open = true,
            _ => self.opaque = true,
        }
    }

    fn of(
        index: &'a Index,
        root: &Root<'a>,
        parent: &[Seg],
        key: &str,
        version: Option<Version>,
    ) -> Self {
        let mut l = Self::default();
        if let (Root::Props(ps), true) = (root, parent.is_empty()) {
            l.props.extend(ps.iter().filter(|p| p.name == key));
            return l;
        }
        let tys = match root {
            Root::Types(ts) if parent.is_empty() => ts.clone(),
            _ => resolve(index, root, parent, version),
        };
        if tys.is_empty() {
            l.opaque = true;
        }
        for t in tys {
            l.add(index, t, key);
        }
        l
    }
}

/// One JSON document the schema applies to: a whole body, or one NDJSON line.
struct Unit<'a> {
    root: Root<'a>,
    /// Document text and its byte offset in the editor text.
    text: String,
    base: usize,
}

/// The body with comment lines blanked out (same byte length), so offsets stay valid.
fn mask_comments(body: &str) -> String {
    body.split_inclusive('\n')
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with('#') || t.starts_with("//") {
                l.chars()
                    .map(|c| {
                        if c == '\n' {
                            "\n".into()
                        } else {
                            " ".repeat(c.len_utf8())
                        }
                    })
                    .collect()
            } else {
                l.to_string()
            }
        })
        .collect()
}

fn units<'a>(text: &str, block: &Block, endpoint: &'a Endpoint) -> Vec<Unit<'a>> {
    let body = mask_comments(&text[block.body.clone()]);
    let ends: Vec<usize> = if endpoint.lines.is_empty() {
        vec![body.len()]
    } else {
        body.match_indices('\n')
            .map(|(i, _)| i)
            .chain([body.len()])
            .collect()
    };
    ends.into_iter()
        .filter_map(move |end| {
            let prefix = &body[..end];
            let (root, current) = body_root(endpoint, prefix).ok()?;
            (!current.trim().is_empty()).then(|| Unit {
                root,
                base: block.body.start + end - current.len(),
                text: current.to_string(),
            })
        })
        .collect()
}

/// Classifies object keys by the schema: `Property` for keywords the DSL declares, `Field` for names
/// the user chose (a field under `match`, an aggregation name). Other keys are left out.
pub fn key_tokens(
    index: &Index,
    text: &str,
    version: Option<Version>,
) -> Vec<(Range<usize>, Token)> {
    let mut out = Vec::new();
    for block in split_blocks(text) {
        let Some(endpoint) = match_endpoint(index, &block.method, &block.path, version) else {
            continue;
        };
        for unit in units(text, &block, endpoint) {
            for tok in keys(&unit.text) {
                let l = Lookup::of(index, &unit.root, &tok.parent, &tok.name, version);
                let kind = if !l.props.is_empty() {
                    Token::Property
                } else if l.open {
                    Token::Field
                } else {
                    continue;
                };
                out.push((unit.base + tok.range.start..unit.base + tok.range.end, kind));
            }
        }
    }
    out
}

/// Everything wrong that can be told without a cluster.
pub fn diagnostics(index: &Index, text: &str, version: Option<Version>) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for block in split_blocks(text) {
        let line = &text[block.line.clone()];
        let line_range = block.line.start..block.line.start + line.trim_end().len();
        let Some(endpoint) = match_endpoint(index, &block.method, &block.path, version) else {
            if block.path.is_empty() {
                continue;
            }
            let message = if match_endpoint(index, &block.method, &block.path, None).is_some() {
                format!(
                    "`{} {}` is not available in this version",
                    block.method, block.path
                )
            } else {
                format!("No endpoint matches `{} {}`", block.method, block.path)
            };
            out.push(Diagnostic {
                range: line_range,
                severity: Severity::Warning,
                message,
            });
            continue;
        };
        query_params(&block, line, endpoint, &mut out);
        for unit in units(text, &block, endpoint) {
            if let Some(d) = syntax_error(&unit) {
                out.push(d);
            }
            for tok in keys(&unit.text) {
                let l = Lookup::of(index, &unit.root, &tok.parent, &tok.name, version);
                let range = unit.base + tok.range.start..unit.base + tok.range.end;
                let (severity, message) =
                    if let Some(p) = l.props.iter().find(|p| visible(p.since, version)) {
                        if !p.deprecated {
                            continue;
                        }
                        (Severity::Warning, format!("`{}` is deprecated", tok.name))
                    } else if let Some(p) = l.props.first() {
                        let since = p
                            .since
                            .map(|v| v.map(|n| n.to_string()).join("."))
                            .unwrap_or_default();
                        (
                            Severity::Warning,
                            format!("`{}` needs version {since} or later", tok.name),
                        )
                    } else if l.open || l.opaque {
                        continue;
                    } else {
                        (
                            Severity::Warning,
                            format!("Unknown property `{}`", tok.name),
                        )
                    };
                out.push(Diagnostic {
                    range,
                    severity,
                    message,
                });
            }
        }
    }
    out
}

fn query_params(block: &Block, line: &str, endpoint: &Endpoint, out: &mut Vec<Diagnostic>) {
    let (Some(q), Some(at)) = (&block.query, line.find('?')) else {
        return;
    };
    // An endpoint with no listed parameters is one the spec has no request type for.
    if endpoint.query.is_empty() {
        return;
    }
    let mut offset = block.line.start + at + 1;
    for kv in q.trim_end().split('&') {
        let name = kv.split('=').next().unwrap_or_default();
        if !name.is_empty() && !endpoint.query.iter().any(|p| p.name == name) {
            out.push(Diagnostic {
                range: offset..offset + name.len(),
                severity: Severity::Warning,
                message: format!("Unknown parameter `{name}`"),
            });
        }
        offset += kv.len() + 1;
    }
}

fn syntax_error(unit: &Unit) -> Option<Diagnostic> {
    let err = serde_json::from_str::<serde::de::IgnoredAny>(&unit.text).err()?;
    let line_start: usize = unit
        .text
        .split_inclusive('\n')
        .take(err.line().saturating_sub(1))
        .map(str::len)
        .sum();
    let mut at = (line_start + err.column().saturating_sub(1)).min(unit.text.len());
    if at == unit.text.len() {
        at = unit.text.trim_end().len().saturating_sub(1);
    }
    while !unit.text.is_char_boundary(at) {
        at -= 1;
    }
    let width = unit.text[at..].chars().next().map_or(0, char::len_utf8);
    let message = err.to_string();
    let message = message.split(" at line ").next().unwrap_or(&message);
    Some(Diagnostic {
        range: unit.base + at..unit.base + at + width.max(1),
        severity: Severity::Error,
        message: message.to_string(),
    })
}

/// Documentation for the key (or request line) at byte offset `cursor`.
pub fn hover(index: &Index, text: &str, cursor: usize, version: Option<Version>) -> Option<Hover> {
    let block = split_blocks(text)
        .into_iter()
        .find(|b| cursor >= b.line.start && cursor <= b.body.end)?;
    let endpoint = match_endpoint(index, &block.method, &block.path, version)?;
    if block.line.contains(&cursor) {
        return Some(Hover {
            range: block.line.start..block.line.start + text[block.line.clone()].trim_end().len(),
            markdown: endpoint_doc(endpoint),
        });
    }
    for unit in units(text, &block, endpoint) {
        for tok in keys(&unit.text) {
            let range = unit.base + tok.range.start..unit.base + tok.range.end;
            if !range.contains(&cursor) {
                continue;
            }
            let l = Lookup::of(index, &unit.root, &tok.parent, &tok.name, version);
            let p = l
                .props
                .iter()
                .find(|p| p.doc.is_some())
                .or(l.props.first())?;
            return Some(Hover {
                range,
                markdown: prop_doc(&l.props, p),
            });
        }
    }
    None
}

fn endpoint_doc(e: &Endpoint) -> String {
    let mut s = format!("**{}**", e.name);
    for u in &e.urls {
        s.push_str(&format!("\n\n`{} {}`", u.methods.join("|"), u.path));
    }
    if let Some(v) = e.since {
        s.push_str(&format!("\n\nSince {}", join_version(v)));
    }
    s
}

fn prop_doc(all: &[&Prop], p: &Prop) -> String {
    let mut types: Vec<String> = all.iter().filter_map(|p| describe(&p.ty)).collect();
    types.dedup();
    let mut s = format!("**{}**", p.name);
    if !types.is_empty() {
        s.push_str(&format!(": `{}`", types.join(" | ")));
    }
    if let Some(d) = &p.doc {
        s.push_str(&format!("\n\n{d}"));
    }
    if let Some(d) = &p.default {
        s.push_str(&format!("\n\nDefault: `{d}`"));
    }
    if let Some(v) = p.since {
        s.push_str(&format!("\n\nSince {}", join_version(v)));
    }
    if p.deprecated {
        s.push_str("\n\nDeprecated");
    }
    s
}

fn join_version(v: Version) -> String {
    v.map(|n| n.to_string()).join(".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OpenKey;
    use std::collections::BTreeMap;

    fn named(n: &str) -> TypeRef {
        TypeRef::Named(n.into())
    }

    fn index() -> Index {
        let mut old = Prop::new("old", TypeRef::Bool);
        old.deprecated = true;
        let mut query = Prop::new("query", named("Query"));
        query.doc = Some("The query to run.".into());
        query.default = Some("{}".into());
        let mut new = Prop::new("fresh", TypeRef::Bool);
        new.since = Some([8, 5, 0]);
        Index {
            endpoints: vec![
                Endpoint {
                    name: "search".into(),
                    urls: vec![crate::Url {
                        path: "/{index}/_search".into(),
                        methods: vec!["POST".into()],
                    }],
                    body: vec![query, Prop::new("size", TypeRef::Other), old, new],
                    query: vec![Prop::new("pretty", TypeRef::Bool)],
                    ..Endpoint::default()
                },
                Endpoint {
                    name: "bulk".into(),
                    urls: vec![crate::Url {
                        path: "/_bulk".into(),
                        methods: vec!["POST".into()],
                    }],
                    lines: vec![named("Action"), TypeRef::Other],
                    ..Endpoint::default()
                },
            ],
            types: BTreeMap::from([
                (
                    "Query".into(),
                    vec![
                        Prop::new("match", TypeRef::FieldDict(Box::new(TypeRef::Other))),
                        Prop::new("bool", named("Bool")),
                    ],
                ),
                (
                    "Bool".into(),
                    vec![Prop::new("must", TypeRef::Array(Box::new(named("Query"))))],
                ),
                ("Action".into(), vec![Prop::new("index", TypeRef::Other)]),
                ("Free".into(), Vec::new()),
            ]),
            open: BTreeMap::from([(
                "Free".into(),
                OpenKey {
                    is_field: false,
                    value: TypeRef::Other,
                },
            )]),
            ..Index::default()
        }
    }

    /// Diagnostic messages with the text each one underlines.
    fn found(text: &str, version: Option<Version>) -> Vec<(String, String)> {
        diagnostics(&index(), text, version)
            .into_iter()
            .map(|d| (d.message, text[d.range].to_string()))
            .collect()
    }

    fn pair(m: &str, t: &str) -> (String, String) {
        (m.to_string(), t.to_string())
    }

    #[test]
    fn valid_requests_are_clean() {
        let ok = [
            "POST i/_search?pretty\n{\"query\":{\"bool\":{\"must\":[{\"match\":{\"any_field\":\"x\"}}]}},\"size\":1}",
            "POST i/_search\n# a comment\n{\"size\": 1}",
            "POST _bulk\n{\"index\":{}}\n{\"anything\":1}\n",
            "",
        ];
        for text in ok {
            assert_eq!(found(text, None), vec![], "{text:?}");
        }
    }

    #[test]
    fn reports_unknown_deprecated_and_newer_properties() {
        let text = "POST i/_search\n{\"qeury\":{},\"old\":true,\"fresh\":true,\"query\":{\"bool\":{\"shuld\":[]}}}";
        assert_eq!(
            found(text, Some([8, 0, 0])),
            vec![
                pair("Unknown property `qeury`", "\"qeury\""),
                pair("`old` is deprecated", "\"old\""),
                pair("`fresh` needs version 8.5.0 or later", "\"fresh\""),
                pair("Unknown property `shuld`", "\"shuld\""),
            ]
        );
        assert_eq!(found(text, Some([8, 5, 0])).len(), 3);
    }

    #[test]
    fn reports_unknown_endpoints_and_parameters() {
        assert_eq!(
            found("POST a/_nope", None),
            vec![pair("No endpoint matches `POST a/_nope`", "POST a/_nope")]
        );
        assert_eq!(
            found("POST a/_search?pretty&bad=1&x", None),
            vec![
                pair("Unknown parameter `bad`", "bad"),
                pair("Unknown parameter `x`", "x"),
            ]
        );
    }

    #[test]
    fn reports_syntax_errors_per_document() {
        let got = found("POST i/_search\n{\"size\": }", None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "}");
        let got = found(
            "POST _bulk\n{\"index\":{}}\n{\"a\":\n{\"index\":{}}\n",
            None,
        );
        assert_eq!(got.len(), 1, "{got:?}");
        // EOF inside the line: the underline lands on its last character.
        assert_eq!(got[0].1, ":");
    }

    #[test]
    fn hover_shows_the_property_card() {
        let text = "POST i/_search\n{\"query\":{}}";
        let at = text.find("query").unwrap() + 1;
        let h = hover(&index(), text, at, None).unwrap();
        assert_eq!(&text[h.range], "\"query\"");
        assert_eq!(
            h.markdown,
            "**query**: `Query`\n\nThe query to run.\n\nDefault: `{}`"
        );
        assert!(hover(&index(), text, text.len() - 1, None).is_none());
        let line = hover(&index(), text, 2, None).unwrap();
        assert!(
            line.markdown
                .starts_with("**search**\n\n`POST /{index}/_search`")
        );
    }

    #[test]
    fn never_panics_on_any_prefix() {
        let doc = "POST i/_search?pretty&x\n# c\n{\"query\":{\"bool\":{\"must\":[{\"match\":{\"f\":\"é\\\"b\"}}]}},\"size\":1}\nPOST _bulk\n{\"index\":{}}\n{\"中\":1}\n";
        let idx = index();
        for (i, _) in doc.char_indices().chain([(doc.len(), ' ')]) {
            let text = &doc[..i];
            diagnostics(&idx, text, None);
            hover(&idx, text, i, None);
        }
    }
}
