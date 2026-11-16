//! Read-only listings of cluster objects beyond indices and aliases: nodes, index templates,
//! running tasks and ingest pipelines. Each item carries the request that inspects it.

use serde_json::Value;

use crate::{Client, Request};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    /// The request that shows this object.
    pub request: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    pub nodes: Vec<Item>,
    pub templates: Vec<Item>,
    pub tasks: Vec<Item>,
    pub pipelines: Vec<Item>,
}

/// `GET {prefix}{name}` with the name escaped, so spaces or `/` in it stay inside one path segment.
fn item(label: &str, prefix: &str, name: &str) -> Item {
    let mut path = String::new();
    for b in name.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' | b'*' => {
                path.push(b as char)
            }
            _ => path.push_str(&format!("%{b:02X}")),
        }
    }
    Item {
        label: label.into(),
        request: format!("GET {prefix}{path}"),
    }
}

fn json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

fn sorted(mut items: Vec<Item>) -> Vec<Item> {
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// From `GET _cat/nodes?format=json&h=name`.
pub fn parse_nodes(body: &str) -> Vec<Item> {
    let names = json(body);
    let names = names.as_array().into_iter().flatten();
    let items = names
        .filter_map(|n| n["name"].as_str())
        .map(|n| item(n, "_nodes/", n))
        .collect();
    sorted(items)
}

/// From `GET _index_template` (composable) and `GET _template` (legacy); either may be empty.
pub fn parse_templates(composable: &str, legacy: &str) -> Vec<Item> {
    let composable = json(composable);
    let new = composable["index_templates"]
        .as_array()
        .into_iter()
        .flatten();
    let mut items: Vec<Item> = new
        .filter_map(|t| t["name"].as_str())
        .map(|n| item(n, "_index_template/", n))
        .collect();
    if let Some(old) = json(legacy).as_object() {
        items.extend(old.keys().map(|n| item(n, "_template/", n)));
    }
    sorted(items)
}

/// From `GET _tasks`: one item per running task, labelled with its action.
pub fn parse_tasks(body: &str) -> Vec<Item> {
    let body = json(body);
    let nodes = body["nodes"].as_object().into_iter().flatten();
    let mut items = Vec::new();
    for tasks in nodes.filter_map(|(_, n)| n["tasks"].as_object()) {
        for (id, t) in tasks {
            let action = t["action"].as_str().unwrap_or("task");
            items.push(item(&format!("{action} · {id}"), "_tasks/", id));
        }
    }
    sorted(items)
}

/// From `GET _ingest/pipeline`.
pub fn parse_pipelines(body: &str) -> Vec<Item> {
    let body = json(body);
    let names = body.as_object().into_iter().flat_map(|o| o.keys());
    sorted(names.map(|n| item(n, "_ingest/pipeline/", n)).collect())
}

impl Catalog {
    /// Reads every listing; one that fails (no permission, old version) is left empty.
    pub async fn fetch(client: &Client) -> Self {
        let get = async |path: &str| {
            let sent = client
                .send(Request {
                    method: "GET".into(),
                    path: path.into(),
                    body: None,
                })
                .await;
            match sent {
                Ok(r) if r.status == 200 => r.body,
                _ => String::new(),
            }
        };
        let (composable, legacy) = (get("_index_template").await, get("_template").await);
        Self {
            nodes: parse_nodes(&get("_cat/nodes?format=json&h=name").await),
            templates: parse_templates(&composable, &legacy),
            tasks: parse_tasks(&get("_tasks").await),
            pipelines: parse_pipelines(&get("_ingest/pipeline").await),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn nodes_and_pipelines() {
        let nodes = parse_nodes(r#"[{"name":"n2"},{"name":"n1"}]"#);
        assert_eq!(labels(&nodes), ["n1", "n2"]);
        assert_eq!(nodes[0].request, "GET _nodes/n1");
        let p = parse_pipelines(r#"{"b":{},"a":{}}"#);
        assert_eq!(p[0].request, "GET _ingest/pipeline/a");
        assert!(parse_nodes("not json").is_empty());
        let spaced = parse_nodes(r#"[{"name":"my node/1"}]"#);
        assert_eq!(spaced[0].request, "GET _nodes/my%20node%2F1");
    }

    #[test]
    fn templates_come_from_both_apis() {
        let t = parse_templates(
            r#"{"index_templates":[{"name":"new"}]}"#,
            r#"{"old":{"index_patterns":["x*"]}}"#,
        );
        assert_eq!(labels(&t), ["new", "old"]);
        assert_eq!(t[0].request, "GET _index_template/new");
        assert_eq!(t[1].request, "GET _template/old");
    }

    #[test]
    fn tasks_are_flattened_across_nodes() {
        let t = parse_tasks(
            r#"{"nodes":{"n":{"tasks":{"n:7":{"action":"indices:data/read/search"}}}}}"#,
        );
        assert_eq!(labels(&t), ["indices:data/read/search · n:7"]);
        assert_eq!(t[0].request, "GET _tasks/n:7");
    }
}
