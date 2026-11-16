//! What a request does to the cluster, judged from its method, path and body.
//!
//! The spec does not mark endpoints as read or write, so `READS` is a small hand-kept list.
//! shortcut: an endpoint missing from it counts as a write and is blocked in readonly mode; add it
//! there when a real read is blocked.

use serde_json::Value;

/// Path segments of endpoints that read although they use POST or PUT.
const READS: &[&str] = &[
    "_search",
    "_msearch",
    "_count",
    "_mget",
    "_mtermvectors",
    "_termvectors",
    "_explain",
    "_validate",
    "_analyze",
    "_field_caps",
    "_terms_enum",
    "_knn_search",
    "_rank_eval",
    "_sql",
    "_eql",
    "_render",
    "_search_shards",
    "_resolve",
];

/// A request that needs an explicit confirmation before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Danger {
    /// The question to ask, naming the target.
    pub title: String,
    /// The confirm button: the specific outcome.
    pub action: &'static str,
}

fn segments(path: &str) -> Vec<&str> {
    let path = path.split('?').next().unwrap_or(path);
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// True when the request may change data or cluster state.
pub fn is_write(method: &str, path: &str) -> bool {
    if matches!(method, "GET" | "HEAD") {
        return false;
    }
    // `_search/template` and `_msearch/template` end in `template`, so look at every segment.
    !segments(path).iter().any(|s| READS.contains(s))
}

fn bulk_deletes(body: &str) -> bool {
    body.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with('{')
            && serde_json::from_str::<Value>(l)
                .is_ok_and(|v| v.as_object().is_some_and(|o| o.contains_key("delete")))
    })
}

/// The confirmation a request needs, if any.
pub fn danger(method: &str, path: &str, body: Option<&str>) -> Option<Danger> {
    let segs = segments(path);
    let target = match segs.first() {
        Some(s) if !s.starts_with('_') => format!("\"{s}\""),
        _ => "the cluster".to_string(),
    };
    let has = |name: &str| segs.contains(&name);
    let (title, action) = if has("_delete_by_query") {
        (
            format!("Delete documents from {target} by query?"),
            "Delete documents",
        )
    } else if has("_update_by_query") {
        (
            format!("Update documents in {target} by query?"),
            "Update documents",
        )
    } else if has("_close") {
        (format!("Close {target}?"), "Close")
    } else if has("_forcemerge") {
        (format!("Force merge {target}?"), "Force merge")
    } else if has("_bulk") && body.is_some_and(bulk_deletes) {
        (
            format!("Run a bulk request that deletes documents in {target}?"),
            "Run bulk request",
        )
    } else if method == "DELETE" {
        (format!("Delete {target}?"), "Delete")
    } else {
        return None;
    };
    Some(Danger { title, action })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_follow_the_endpoint_not_the_method() {
        let cases = [
            ("GET", "idx/_doc/1", false),
            ("HEAD", "idx", false),
            ("POST", "idx/_search?size=1", false),
            ("POST", "_msearch", false),
            ("POST", "idx/_search/template", false),
            ("POST", "idx/_doc", true),
            ("PUT", "idx", true),
            ("DELETE", "idx", true),
            ("POST", "idx/_delete_by_query", true),
            ("POST", "_unknown_thing", true),
        ];
        for (m, p, write) in cases {
            assert_eq!(is_write(m, p), write, "{m} {p}");
        }
    }

    #[test]
    fn dangerous_requests_name_their_target() {
        let d = |m, p, b| danger(m, p, b).map(|d| (d.title, d.action));
        assert_eq!(
            d("DELETE", "/logs-1", None),
            Some(("Delete \"logs-1\"?".into(), "Delete"))
        );
        assert_eq!(
            d("POST", "logs/_delete_by_query?conflicts=proceed", None)
                .unwrap()
                .1,
            "Delete documents"
        );
        assert_eq!(d("POST", "logs/_close", None).unwrap().1, "Close");
        assert_eq!(
            d("POST", "_forcemerge", None).unwrap().0,
            "Force merge the cluster?"
        );
        let bulk = "{\"index\":{}}\n{\"a\":1}\n{\"delete\":{\"_id\":\"1\"}}\n";
        assert!(d("POST", "idx/_bulk", Some(bulk)).is_some());
        assert!(d("POST", "idx/_bulk", Some("{\"index\":{}}\n{\"a\":1}\n")).is_none());
        assert!(d("POST", "idx/_search", None).is_none());
        assert!(d("PUT", "idx", None).is_none());
    }
}
