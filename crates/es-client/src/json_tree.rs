//! A response body as a tree of labelled nodes, each addressed by a JSONPath-style path.

use serde_json::Value;

/// A body with more nodes than this is cut; the node that hit the limit says how much is missing.
const MAX_NODES: usize = 20_000;

/// Strings longer than this are cut in the label; the JSON view has the whole value.
const MAX_LABEL: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// `$`, `$.hits.hits[0]`, `$["a.b"]`: unique in the tree, and what "copy path" yields.
    pub path: String,
    /// `key: value` for a scalar, `key: {3}` or `key: [2]` for a container.
    pub label: String,
    pub children: Vec<Node>,
}

/// `None` when `body` is not JSON.
pub fn json_tree(body: &str) -> Option<Node> {
    let value: Value = serde_json::from_str(body).ok()?;
    let mut budget = MAX_NODES;
    Some(build("$".into(), String::new(), &value, &mut budget))
}

fn build(path: String, key: String, v: &Value, budget: &mut usize) -> Node {
    *budget = budget.saturating_sub(1);
    let prefix = if key.is_empty() {
        String::new()
    } else {
        format!("{key}: ")
    };
    let (summary, kids): (String, Vec<(String, String, &Value)>) = match v {
        Value::Object(o) => (
            format!("{{{}}}", o.len()),
            o.iter()
                .map(|(k, c)| (child_path(&path, k), k.clone(), c))
                .collect(),
        ),
        Value::Array(a) => (
            format!("[{}]", a.len()),
            a.iter()
                .enumerate()
                .map(|(i, c)| (format!("{path}[{i}]"), format!("[{i}]"), c))
                .collect(),
        ),
        Value::String(s) => (format!("{:?}", cut(s)), Vec::new()),
        other => (other.to_string(), Vec::new()),
    };
    let total = kids.len();
    let mut children = Vec::new();
    for (p, k, c) in kids {
        if *budget == 0 {
            let more = total - children.len();
            children.push(Node {
                path: format!("{path}#more"),
                label: format!("… {more} more not shown"),
                children: Vec::new(),
            });
            break;
        }
        children.push(build(p, k, c, budget));
    }
    Node {
        path,
        label: format!("{prefix}{summary}"),
        children,
    }
}

fn child_path(parent: &str, key: &str) -> String {
    let plain = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if plain {
        format!("{parent}.{key}")
    } else {
        format!("{parent}[{key:?}]")
    }
}

fn cut(s: &str) -> String {
    match s.char_indices().nth(MAX_LABEL) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_paths() {
        let t = json_tree(r#"{"took":3,"hits":{"hits":[{"_id":"a"}]},"a.b":null}"#).unwrap();
        assert_eq!((t.path.as_str(), t.label.as_str()), ("$", "{3}"));
        let paths: Vec<&str> = t.children.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["$.took", "$.hits", "$[\"a.b\"]"]);
        assert_eq!(t.children[0].label, "took: 3");
        assert_eq!(t.children[2].label, "a.b: null");
        let id = &t.children[1].children[0].children[0].children[0];
        assert_eq!(
            (id.path.as_str(), id.label.as_str()),
            ("$.hits.hits[0]._id", "_id: \"a\"")
        );
        assert_eq!(t.children[1].children[0].label, "hits: [1]");
    }

    #[test]
    fn rejects_non_json_and_caps_huge_bodies() {
        assert!(json_tree("nope").is_none());
        let big = format!("[{}]", vec!["1"; MAX_NODES + 50].join(","));
        let t = json_tree(&big).unwrap();
        let last = t.children.last().unwrap();
        assert!(last.label.starts_with("… 5") && last.label.ends_with("more not shown"));
    }
}
