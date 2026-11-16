//! Cluster metadata for completion: index/alias names and per-target mappings.
//!
//! [`Fields`] is called synchronously from the completion path, so it only reads the cache and
//! records which targets it lacked; [`ClusterMeta::refresh`] (async) fills them in.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use es_dsl_core::{Field, Fields, parse_aliases, parse_mapping, parse_names};

use crate::{Client, Request};

#[derive(Default)]
struct State {
    names: Vec<String>,
    aliases: Vec<String>,
    mappings: HashMap<String, Vec<Field>>,
    wanted: HashSet<String>,
    names_at: Option<Instant>,
}

/// Index and alias names are re-read at most this often.
const NAMES_TTL: Duration = Duration::from_secs(30);

/// Shared cache; clones see the same data.
#[derive(Clone, Default)]
pub struct ClusterMeta(Arc<Mutex<State>>);

impl ClusterMeta {
    /// Reloads the names and fetches the mapping of every target requested since the last refresh.
    /// A failed call leaves the previous data in place; the first error is returned.
    pub async fn refresh(&self, client: &Client) -> crate::Result<()> {
        let get = |path: &str| {
            client.send(Request {
                method: "GET".into(),
                path: path.into(),
                body: None,
            })
        };
        let stale = self.lock().names_at.is_none_or(|t| t.elapsed() > NAMES_TTL);
        if stale {
            let cat = get("_cat/indices?format=json&h=index").await?;
            let alias = get("_alias").await?;
            let ok = |r: crate::Response| {
                if r.status == 200 {
                    r.body
                } else {
                    String::new()
                }
            };
            let (cat, alias) = (ok(cat), ok(alias));
            let (names, aliases) = (parse_names(&cat, &alias), parse_aliases(&alias));
            let mut s = self.lock();
            s.names = names;
            s.aliases = aliases;
            s.names_at = Some(Instant::now());
        }
        let wanted: Vec<String> = self.lock().wanted.drain().collect();
        let mut first_err = None;
        for target in wanted {
            match get(&format!("{target}/_mapping")).await {
                Ok(r) if r.status == 200 => {
                    self.lock().mappings.insert(target, parse_mapping(&r.body));
                }
                Ok(_) => {}
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    /// Makes the next refresh re-read index and alias names regardless of age.
    pub fn invalidate(&self) {
        self.lock().names_at = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Only plain index expressions are fetched, so a typed path can never smuggle in other segments.
fn fetchable(target: &str) -> bool {
    !target.is_empty()
        && target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.*,".contains(c))
}

impl Fields for ClusterMeta {
    fn fields(&self, target: &str) -> Vec<Field> {
        let mut s = self.lock();
        if let Some(f) = s.mappings.get(target) {
            return f.clone();
        }
        if fetchable(target) {
            s.wanted.insert(target.into());
        }
        Vec::new()
    }

    fn names(&self) -> Vec<String> {
        self.lock().names.clone()
    }

    fn aliases(&self) -> Vec<String> {
        self.lock().aliases.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    use httpmock::prelude::*;

    #[tokio::test]
    async fn fills_names_and_mappings_after_a_miss() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/_cat/indices");
            then.status(200).body(r#"[{"index":"logs-1"}]"#);
        });
        server.mock(|when, then| {
            when.method(GET).path("/_alias");
            then.status(200)
                .body(r#"{"logs-1":{"aliases":{"logs":{}}}}"#);
        });
        server.mock(|when, then| {
            when.method(GET).path("/logs/_mapping");
            then.status(200)
                .body(r#"{"logs-1":{"mappings":{"properties":{"msg":{"type":"text"}}}}}"#);
        });
        let client = Client::new(Config::new(server.base_url())).unwrap();
        let meta = ClusterMeta::default();

        assert!(meta.fields("logs").is_empty(), "a miss never blocks");
        assert!(meta.fields("../x?").is_empty());
        meta.refresh(&client).await.unwrap();

        assert_eq!(meta.names(), ["logs", "logs-1"]);
        assert_eq!(meta.aliases(), ["logs"]);
        let f = meta.fields("logs");
        assert_eq!((f[0].name.as_str(), f[0].ty.as_str()), ("msg", "text"));
        assert!(!meta.lock().wanted.contains("../x?"));
    }
}
