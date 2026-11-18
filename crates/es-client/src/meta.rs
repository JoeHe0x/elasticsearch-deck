//! Cluster metadata for completion: index/alias names and per-target mappings.
//!
//! [`Fields`] is called synchronously from the completion path, so it only reads the cache and
//! records which targets it lacked; [`ClusterMeta::refresh`] (async) fills them in.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use es_dsl_core::{Field, Fields, parse_aliases, parse_mapping, parse_names};
use tokio::sync::Notify;

use crate::{Client, Request};

/// A cached mapping and when it was read. `at` is `None` after [`ClusterMeta::invalidate`]: the
/// fields are still served, so completion keeps working while they are re-read.
struct Mapping {
    fields: Arc<[Field]>,
    at: Option<Instant>,
}

#[derive(Default)]
struct State {
    names: Vec<String>,
    aliases: Vec<String>,
    closed: Vec<String>,
    mappings: HashMap<String, Mapping>,
    wanted: HashSet<String>,
    names_at: Option<Instant>,
}

/// Index and alias names are re-read at most this often.
const NAMES_TTL: Duration = Duration::from_secs(60);

/// Mappings change much less often than names. A cached mapping older than this is still served,
/// but a completion that uses it also asks for a background re-read.
const MAPPING_TTL: Duration = Duration::from_secs(60);

/// Shared cache; clones see the same data.
#[derive(Clone, Default)]
pub struct ClusterMeta {
    state: Arc<Mutex<State>>,
    /// Wakes the refresh loop when a completion asked for a mapping that is missing or stale, so
    /// the first request against a new target does not wait for the next periodic tick.
    wake: Arc<Notify>,
}

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
            let cat = get("_cat/indices?format=json&h=index,status").await?;
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
            let closed = parse_closed(&cat);
            let mut s = self.lock();
            s.closed = closed;
            s.names = names;
            s.aliases = aliases;
            s.names_at = Some(Instant::now());
        }
        let wanted: Vec<String> = self.lock().wanted.drain().collect();
        let mut first_err = None;
        for target in wanted {
            match get(&format!("{target}/_mapping")).await {
                Ok(r) if r.status == 200 => {
                    let mapping = Mapping {
                        fields: Arc::from(parse_mapping(&r.body)),
                        at: Some(Instant::now()),
                    };
                    self.lock().mappings.insert(target, mapping);
                }
                Ok(_) => {}
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    /// Resolves when completion asked for a mapping that is missing or stale. The owner should call
    /// [`ClusterMeta::refresh`] when this returns instead of waiting for its periodic tick.
    pub async fn wanted(&self) {
        self.wake.notified().await;
    }

    /// The indices that are closed, from the last name read.
    pub fn closed(&self) -> Vec<String> {
        self.lock().closed.clone()
    }

    /// The mapping fields of `target` when they have been read; `None` when they have not
    /// arrived yet. Unlike [`Fields::fields`] this never asks for a fetch, so a display that
    /// repaints can read it every frame without keeping the refresh loop awake. A cached mapping
    /// may still be stale.
    pub fn cached(&self, target: &str) -> Option<Arc<[Field]>> {
        self.lock()
            .mappings
            .get(target)
            .map(|mapping| mapping.fields.clone())
    }

    /// Makes the next refresh re-read index and alias names and re-fetch every mapping, while
    /// keeping the cached mappings usable until the new ones arrive.
    pub fn invalidate(&self) {
        let mut s = self.lock();
        s.names_at = None;
        for mapping in s.mappings.values_mut() {
            mapping.at = None;
        }
    }

    /// Records `target` for the next refresh and wakes the loop. Callers that show a mapping
    /// without completing a request — the tree's expanded rows — ask the same way completion
    /// does; a miss is served on the next refresh instead of leaving the callers blank.
    pub fn request(&self, target: &str) {
        let wake = self.request_locked(&mut self.lock(), target);
        // Wake only after the lock is dropped: the loop takes it on its first line.
        if wake {
            self.wake.notify_one();
        }
    }

    /// Records `target` for the next refresh; true when it was not already recorded.
    fn request_locked(&self, s: &mut State, target: &str) -> bool {
        fetchable(target) && s.wanted.insert(target.into())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// From `GET _cat/indices?format=json&h=index,status`: the names whose status is `close`.
fn parse_closed(cat: &str) -> Vec<String> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(cat).unwrap_or_default();
    rows.iter()
        .filter(|r| r["status"] == "close")
        .filter_map(|r| r["index"].as_str().map(String::from))
        .collect()
}

/// Only plain index expressions are fetched, so a typed path can never smuggle in other segments.
fn fetchable(target: &str) -> bool {
    !target.is_empty()
        && target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.*,".contains(c))
}

impl Fields for ClusterMeta {
    fn fields(&self, target: &str) -> Arc<[Field]> {
        let (fields, wake) = {
            let mut s = self.lock();
            match s.mappings.get(target) {
                Some(mapping) => {
                    let fields = mapping.fields.clone();
                    let fresh = mapping.at.is_some_and(|at| at.elapsed() <= MAPPING_TTL);
                    let wake = if fresh {
                        false
                    } else {
                        self.request_locked(&mut s, target)
                    };
                    (fields, wake)
                }
                None => (Arc::from([]), self.request_locked(&mut s, target)),
            }
        };
        // Wake only after the lock is dropped: the loop takes it on its first line.
        if wake {
            self.wake.notify_one();
        }
        fields
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

    #[test]
    fn closed_indices_are_those_with_status_close() {
        let cat = r#"[{"index":"a","status":"open"},{"index":"b","status":"close"}]"#;
        assert_eq!(parse_closed(cat), ["b"]);
        assert!(parse_closed("nope").is_empty());
    }
    use crate::Config;
    use httpmock::prelude::*;

    fn names_mocks(server: &MockServer) {
        server.mock(|when, then| {
            when.method(GET).path("/_cat/indices");
            then.status(200).body(r#"[{"index":"logs-1"}]"#);
        });
        server.mock(|when, then| {
            when.method(GET).path("/_alias");
            then.status(200)
                .body(r#"{"logs-1":{"aliases":{"logs":{}}}}"#);
        });
    }

    fn mapping_mock(server: &MockServer) -> httpmock::Mock<'_> {
        server.mock(|when, then| {
            when.method(GET).path("/logs/_mapping");
            then.status(200)
                .body(r#"{"logs-1":{"mappings":{"properties":{"msg":{"type":"text"}}}}}"#);
        })
    }

    #[tokio::test]
    async fn fills_names_and_mappings_after_a_miss() {
        let server = MockServer::start_async().await;
        names_mocks(&server);
        mapping_mock(&server);
        let client = Client::new(Config::new(server.base_url())).unwrap();
        let meta = ClusterMeta::default();

        assert!(meta.fields("logs").is_empty(), "a miss never blocks");
        assert!(meta.cached("logs").is_none(), "a miss is not a read");
        assert!(meta.fields("../x?").is_empty());
        meta.refresh(&client).await.unwrap();

        assert!(
            meta.cached("logs").is_some(),
            "the refresh filled the mapping in"
        );
        assert_eq!(meta.names(), ["logs", "logs-1"]);
        assert_eq!(meta.aliases(), ["logs"]);
        let f = meta.fields("logs");
        assert_eq!((f[0].name.as_str(), f[0].ty.as_str()), ("msg", "text"));
        assert!(!meta.lock().wanted.contains("../x?"));
    }

    #[tokio::test]
    async fn an_explicit_request_fetches_the_mapping() {
        let server = MockServer::start_async().await;
        names_mocks(&server);
        let mapping = mapping_mock(&server);
        let client = Client::new(Config::new(server.base_url())).unwrap();
        let meta = ClusterMeta::default();

        // A caller that only asks whether the mapping is cached must not be stuck waiting for
        // someone else to request it: the explicit request is the fetch.
        assert!(meta.cached("logs").is_none());
        meta.request("logs");
        meta.refresh(&client).await.unwrap();
        assert!(meta.cached("logs").is_some());
        assert_eq!(meta.fields("logs")[0].name, "msg");
        assert_eq!(mapping.calls(), 1);

        // Only plain index expressions are requested.
        meta.request("../x?");
        meta.refresh(&client).await.unwrap();
        assert!(meta.cached("../x?").is_none());
        assert_eq!(mapping.calls(), 1);
    }

    #[tokio::test]
    async fn a_miss_wakes_the_refresh_loop() {
        let meta = ClusterMeta::default();
        assert!(meta.fields("logs").is_empty());
        // The permission is stored, so the loop's next wait returns without another miss.
        tokio::time::timeout(Duration::from_millis(100), meta.wanted())
            .await
            .expect("a miss must wake the loop even when it was not waiting yet");
    }

    #[tokio::test]
    async fn invalidate_keeps_serving_and_refetches() {
        let server = MockServer::start_async().await;
        names_mocks(&server);
        let mapping = mapping_mock(&server);
        let client = Client::new(Config::new(server.base_url())).unwrap();
        let meta = ClusterMeta::default();
        let _ = meta.fields("logs");
        meta.refresh(&client).await.unwrap();
        assert_eq!(mapping.calls(), 1);

        meta.invalidate();
        // The stale table is still served, so completion does not go blank during the reload.
        assert_eq!(meta.fields("logs")[0].name, "msg");
        meta.refresh(&client).await.unwrap();
        assert_eq!(mapping.calls(), 2);
    }
}
