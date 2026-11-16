//! Request history: a capped JSON file of what was sent, per connection. Credentials never get here,
//! since they live in the connection, not in the request text; requests to the security APIs are
//! skipped because their bodies carry passwords and API keys.

use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Oldest entries are dropped beyond this many.
pub const MAX_ENTRIES: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub connection: String,
    pub method: String,
    /// Includes the query string, as typed.
    pub path: String,
    pub body: Option<String>,
    /// Unix seconds.
    pub at: u64,
}

/// Appends `entry` (newest last). Returns false when it was not recorded: a repeat of the newest
/// entry, or a request whose body may hold credentials.
pub fn record(entries: &mut Vec<Entry>, entry: Entry) -> bool {
    let sensitive = entry
        .path
        .split(['/', '?'])
        .any(|s| s == "_security" || s == "_xpack");
    let repeat = entries.last().is_some_and(|last| {
        (&last.connection, &last.method, &last.path, &last.body)
            == (&entry.connection, &entry.method, &entry.path, &entry.body)
    });
    if sensitive || repeat {
        return false;
    }
    entries.push(entry);
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
    true
}

/// A missing file is an empty list. Also used for the saved-item file.
pub fn load<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| Error::Config(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(Error::Config(e.to_string())),
    }
}

pub fn save<T: Serialize>(path: &Path, entries: &[T]) -> Result<()> {
    let text = serde_json::to_string(entries).map_err(|e| Error::Config(e.to_string()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::Config(e.to_string()))?;
    }
    std::fs::write(path, text).map_err(|e| Error::Config(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> Entry {
        Entry {
            connection: "c".into(),
            method: "GET".into(),
            path: path.into(),
            body: None,
            at: 0,
        }
    }

    #[test]
    fn skips_repeats_and_security_requests_and_caps_the_list() {
        let mut list = Vec::new();
        assert!(record(&mut list, entry("a")));
        assert!(!record(&mut list, entry("a")), "same as the newest");
        assert!(record(&mut list, entry("b")));
        assert!(record(&mut list, entry("a")), "not the newest any more");
        assert!(!record(&mut list, entry("_security/user/x")));
        assert!(!record(&mut list, entry("/_security")));
        for i in 0..MAX_ENTRIES + 10 {
            record(&mut list, entry(&format!("p{i}")));
        }
        assert_eq!(list.len(), MAX_ENTRIES);
        assert_eq!(list.last().unwrap().path, format!("p{}", MAX_ENTRIES + 9));
    }

    #[test]
    fn file_round_trip() {
        let dir = std::env::temp_dir().join(format!("esdeck-hist-{}", std::process::id()));
        let path = dir.join("history.json");
        assert!(load::<Entry>(&path).unwrap().is_empty());
        save(&path, &[entry("a")]).unwrap();
        assert_eq!(load::<Entry>(&path).unwrap(), [entry("a")]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
