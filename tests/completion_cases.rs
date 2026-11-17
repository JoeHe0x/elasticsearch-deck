//! Benchmark cases: request text with a `|` cursor and the labels that must (not) be offered,
//! run against an index converted from the real `elasticsearch-specification` schema.
//!
//! The spec is too large to commit, so point `ES_SPEC_SCHEMA` at a `schema.json`; without it the
//! test only reports that it did nothing.
//!
//! Case file format, cases separated by a line holding `---`:
//!
//! ```text
//! # name (source of the example)
//! POST idx/_search
//! {"query": {"|
//! has: bool match
//! lacks: _id
//! ```
//!
//! `has:` labels must all be offered, `lacks:` labels must not be. `none:` expects no candidates.

use std::{fs, path::Path, sync::Arc};

use es_dsl_core::candidates_with;
use es_dsl_core::{Field, Fields, parse_version};

struct Mapping;
impl Fields for Mapping {
    fn fields(&self, _target: &str) -> Arc<[Field]> {
        let field = |name: &str, ty: &str, nested: bool| Field {
            name: name.into(),
            ty: ty.into(),
            nested,
        };
        Arc::from([
            field("title", "text", false),
            field("price", "double", false),
            field("@timestamp", "date", false),
            field("user.name", "keyword", false),
            field("user", "object", false),
            field("tags", "nested", false),
            field("tags.name", "keyword", true),
            field("loc", "geo_point", false),
            field("busy", "boolean", false),
            field("notes", "flattened", false),
        ])
    }
}

#[test]
fn completion_cases() {
    let Ok(schema) = std::env::var("ES_SPEC_SCHEMA") else {
        eprintln!("ES_SPEC_SCHEMA is not set: completion cases NOT run");
        return;
    };
    let index = es_deck_schema::convert(&fs::read_to_string(schema).unwrap()).unwrap();
    let version = parse_version("8.15.0");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/completion_cases");
    let mut failures = Vec::new();
    let mut total = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let text = fs::read_to_string(&path).unwrap();
        for case in text.split("\n---\n") {
            total += 1;
            if let Some(why) = run(&index, version, case) {
                failures.push(format!("{}: {why}", path.display()));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{}/{total} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn run(
    index: &es_dsl_core::Index,
    version: Option<es_dsl_core::Version>,
    case: &str,
) -> Option<String> {
    let (mut name, mut body, mut has, mut lacks, mut none) =
        ("", Vec::new(), Vec::new(), Vec::new(), false);
    for line in case.lines() {
        if let Some(n) = line.strip_prefix("# ") {
            name = n;
        } else if let Some(l) = line.strip_prefix("has: ") {
            has = l.split_whitespace().collect();
        } else if let Some(l) = line.strip_prefix("lacks: ") {
            lacks = l.split_whitespace().collect();
        } else if line == "none:" {
            none = true;
        } else {
            body.push(line);
        }
    }
    let text = body.join("\n");
    let cursor = text.find('|').expect("case has no cursor");
    let text = text.replacen('|', "", 1);
    // Every prefix of a real request must also complete without panicking.
    for (i, _) in text.char_indices() {
        let _ = candidates_with(index, &text, i, version, &Mapping);
    }
    let got: Vec<String> = candidates_with(index, &text, cursor, version, &Mapping)
        .map(|c| c.into_iter().map(|c| c.label).collect())
        .unwrap_or_default();
    let missing: Vec<_> = has
        .iter()
        .filter(|h| !got.iter().any(|g| g == *h))
        .collect();
    let extra: Vec<_> = lacks
        .iter()
        .filter(|l| got.iter().any(|g| g == *l))
        .collect();
    if none && !got.is_empty() {
        return Some(format!("{name}: expected none, got {got:?}"));
    }
    if !missing.is_empty() || !extra.is_empty() {
        let head = &got[..got.len().min(8)];
        return Some(format!(
            "{name}: missing {missing:?}, unexpected {extra:?}, got {head:?}"
        ));
    }
    None
}
