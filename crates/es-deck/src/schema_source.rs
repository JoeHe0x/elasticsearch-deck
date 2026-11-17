//! The completion index for the connected cluster's version: a cached copy of the matching
//! `elasticsearch-specification` branch, downloaded on first use. The bundled index stands in until
//! then and whenever the download fails.

use std::path::{Path, PathBuf};
use std::time::Duration;

use es_client::{Client, Config, Request};
use es_dsl_core::{Index, Version};

/// `{branch}` is replaced by a branch name such as `8.19`. The user can point this at a mirror by
/// putting another template in `schema-url.txt` in the config directory.
pub const DEFAULT_URL: &str = "https://raw.githubusercontent.com/elastic/elasticsearch-specification/{branch}/output/schema/schema.json";

/// How many minors below the cluster's to try. The spec has no branch for every minor (7.17 is the
/// last 7.x), so a cluster on 7.19 uses 7.17.
const MINORS_BACK: u32 = 6;

/// Branches to try for a cluster, best first: its own `major.minor`, then lower minors of the same major.
pub fn candidates(version: Version) -> Vec<String> {
    let [major, minor, _] = version;
    (0..=MINORS_BACK.min(minor))
        .map(|back| format!("{major}.{}", minor - back))
        .collect()
}

/// The template in `schema-url.txt`, or the default when there is none.
pub fn url_template(dir: &Path) -> String {
    let custom = std::fs::read_to_string(dir.join("schema-url.txt")).unwrap_or_default();
    let custom = custom.trim();
    if custom.contains("{branch}") {
        custom.to_string()
    } else {
        DEFAULT_URL.to_string()
    }
}

/// Splits `https://host/a/b` into `https://host` and `a/b`.
fn split_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.find("://")? + 3;
    let slash = rest + url[rest..].find('/')?;
    Some((&url[..slash], &url[slash + 1..]))
}

fn cache_path(dir: &Path, branch: &str) -> PathBuf {
    dir.join(format!("index-{branch}.json"))
}

/// The cached index of the best candidate that has one.
fn cached(dir: &Path, branches: &[String]) -> Option<Index> {
    branches.iter().find_map(|branch| {
        let bytes = std::fs::read(cache_path(dir, branch)).ok()?;
        serde_json::from_slice(&bytes).ok()
    })
}

/// A cached index when one exists for any candidate, else the first branch the server has, converted
/// and cached. `None` leaves the caller on the bundled index. Blocking work inside: run it off the UI thread.
pub async fn load(dir: PathBuf, version: Version) -> Option<Index> {
    let branches = candidates(version);
    if let Some(index) = cached(&dir, &branches) {
        return Some(index);
    }
    let template = url_template(&dir);
    for branch in &branches {
        let url = template.replace("{branch}", branch);
        let (base, path) = split_url(&url)?;
        let mut config = Config::new(base);
        config.timeout = Duration::from_secs(120);
        let client = Client::new(config).ok()?;
        let request = Request {
            method: "GET".into(),
            path: path.into(),
            body: None,
        };
        let response = client.send(request).await.ok()?;
        match response.status {
            200 => {
                let index = es_deck_schema::convert(&response.body).ok()?;
                write_cache(&dir, branch, &index);
                return Some(index);
            }
            // No branch for this minor: try the next lower one.
            404 => continue,
            _ => return None,
        }
    }
    None
}

fn write_cache(dir: &Path, branch: &str, index: &Index) {
    let path = cache_path(dir, branch);
    let tmp = path.with_extension("tmp");
    // A failed write only means the next connection downloads again.
    let Ok(bytes) = serde_json::to_vec(index) else {
        return;
    };
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cluster_tries_its_own_minor_then_lower_ones_of_the_same_major() {
        assert_eq!(candidates([8, 11, 3])[..2], ["8.11", "8.10"]);
        assert_eq!(
            candidates([7, 19, 0]),
            ["7.19", "7.18", "7.17", "7.16", "7.15", "7.14", "7.13"]
        );
        assert_eq!(candidates([9, 0, 1]), ["9.0"]);
    }

    #[test]
    fn urls_split_into_host_and_path() {
        assert_eq!(
            split_url("https://mirror.corp/spec/8.19/schema.json"),
            Some(("https://mirror.corp", "spec/8.19/schema.json"))
        );
        assert_eq!(split_url("not a url"), None);
        assert_eq!(split_url("https://host-only"), None);
    }

    #[test]
    fn a_template_without_the_branch_placeholder_is_ignored() {
        let dir = std::env::temp_dir().join("esdeck-schema-url");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("schema-url.txt"),
            "https://mirror/x/{branch}/s.json\n",
        )
        .unwrap();
        assert_eq!(url_template(&dir), "https://mirror/x/{branch}/s.json");
        std::fs::write(dir.join("schema-url.txt"), "https://mirror/x/s.json").unwrap();
        assert_eq!(url_template(&dir), DEFAULT_URL);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_cached_index_is_used_without_the_network() {
        let dir = std::env::temp_dir().join("esdeck-schema-cache");
        std::fs::create_dir_all(&dir).unwrap();
        write_cache(&dir, "7.17", &crate::lsp_client::sample_index());
        // 7.19 has no branch, so its candidates reach the cached 7.17.
        assert!(cached(&dir, &candidates([7, 19, 0])).is_some());
        assert!(cached(&dir, &candidates([8, 5, 0])).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
