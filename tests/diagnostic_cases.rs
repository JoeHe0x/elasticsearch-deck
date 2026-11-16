//! Diagnostic benchmark: complete requests run against an index converted from the real spec.
//! `clean` cases must produce no diagnostics (a guard against false positives on valid requests);
//! `warns: text` cases must produce one diagnostic per line whose message contains the text.
//!
//! Like `completion_cases`, this needs `ES_SPEC_SCHEMA` and reports that it did nothing without it.
//! Cases are separated by a line holding `---`; the first line is `# name`.

use std::{fs, path::Path};

use es_dsl_core::{diagnostics, parse_version};

#[test]
fn diagnostic_cases() {
    let Ok(schema) = std::env::var("ES_SPEC_SCHEMA") else {
        eprintln!("ES_SPEC_SCHEMA is not set: diagnostic cases NOT run");
        return;
    };
    let index = es_deck_schema::convert(&fs::read_to_string(schema).unwrap()).unwrap();
    let version = parse_version("8.15.0");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/diagnostic_cases");
    let mut failures = Vec::new();
    let mut total = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        for case in fs::read_to_string(&path).unwrap().split("\n---\n") {
            total += 1;
            let mut name = "";
            let (mut request, mut expect) = (Vec::new(), Vec::new());
            for line in case.lines() {
                if let Some(n) = line.strip_prefix("# ") {
                    name = n;
                } else if let Some(w) = line.strip_prefix("warns: ") {
                    expect.push(w);
                } else if line != "clean" {
                    request.push(line);
                }
            }
            let text = request.join("\n");
            let got: Vec<String> = diagnostics(&index, &text, version)
                .into_iter()
                .map(|d| format!("{} [{}]", d.message, &text[d.range]))
                .collect();
            let ok =
                got.len() == expect.len() && got.iter().zip(&expect).all(|(g, e)| g.contains(e));
            if !ok {
                failures.push(format!("{name}: expected {expect:?}, got {got:?}"));
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
