//! Saved consoles, requests and queries: a JSON file per user, tagged by connection. The same
//! credential rule as the history applies: text that touches the security APIs is never kept.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The whole editor text.
    Console,
    /// One request that is not a search.
    Request,
    /// One search request (`_search`, `_count`, `_msearch`).
    Query,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub connection: String,
    pub kind: Kind,
    pub name: String,
    pub text: String,
    /// Unix seconds.
    pub at: u64,
}

/// Which folder a single request belongs in.
pub fn request_kind(path: &str) -> Kind {
    let path = path.split('?').next().unwrap_or(path);
    let last = path.rsplit('/').next().unwrap_or(path);
    if matches!(last, "_search" | "_count" | "_msearch") {
        Kind::Query
    } else {
        Kind::Request
    }
}

/// The first non-empty line, cut to a length a list row can show.
pub fn name_of(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    line.trim().chars().take(60).collect()
}

fn sensitive(text: &str) -> bool {
    text.contains("_security") || text.contains("_xpack")
}

/// Adds `item`, replacing an earlier one with the same connection, kind and text.
/// Returns false, keeping nothing, when the text is empty or may hold credentials.
pub fn add(list: &mut Vec<Saved>, item: Saved) -> bool {
    if item.text.trim().is_empty() || sensitive(&item.text) {
        return false;
    }
    list.retain(|s| (&s.connection, s.kind, &s.text) != (&item.connection, item.kind, &item.text));
    list.push(item);
    true
}

/// The console every connection starts with.
pub const AUTO_CONSOLE: &str = "console";

/// Stores `text` as the connection's console called `name`, creating it on first use.
/// Returns false when nothing changed: the text is empty, may hold credentials, or is already stored.
pub fn set_console(
    list: &mut Vec<Saved>,
    connection: &str,
    name: &str,
    text: &str,
    at: u64,
) -> bool {
    if text.trim().is_empty() || sensitive(text) {
        return false;
    }
    let found = list
        .iter_mut()
        .find(|s| s.connection == connection && s.kind == Kind::Console && s.name == name);
    match found {
        Some(s) if s.text == text => false,
        Some(s) => {
            s.text = text.into();
            s.at = at;
            true
        }
        None => {
            list.push(Saved {
                connection: connection.into(),
                kind: Kind::Console,
                name: name.into(),
                text: text.into(),
                at,
            });
            true
        }
    }
}

/// Renames the saved item with this connection, kind and text. False for an empty name or no match.
pub fn rename(list: &mut [Saved], connection: &str, kind: Kind, text: &str, name: &str) -> bool {
    let name = name.trim();
    let found = list
        .iter_mut()
        .find(|s| (s.connection.as_str(), s.kind, s.text.as_str()) == (connection, kind, text));
    match found {
        Some(s) if !name.is_empty() => {
            s.name = name.chars().take(60).collect();
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(kind: Kind, text: &str) -> Saved {
        Saved {
            connection: "c".into(),
            kind,
            name: name_of(text),
            text: text.into(),
            at: 0,
        }
    }

    #[test]
    fn searches_go_to_queries_and_the_rest_to_requests() {
        assert_eq!(request_kind("logs/_search?size=1"), Kind::Query);
        assert_eq!(request_kind("_count"), Kind::Query);
        assert_eq!(request_kind("_cat/indices"), Kind::Request);
        assert_eq!(request_kind("logs/_doc/1"), Kind::Request);
    }

    #[test]
    fn add_replaces_a_repeat_and_refuses_credentials() {
        let mut list = Vec::new();
        assert!(add(&mut list, saved(Kind::Request, "GET _cat/nodes")));
        assert!(add(&mut list, saved(Kind::Request, "GET _cat/nodes")));
        assert_eq!(list.len(), 1);
        assert!(!add(
            &mut list,
            saved(Kind::Console, "PUT _security/user/x\n{}")
        ));
        assert!(!add(&mut list, saved(Kind::Console, "  \n")));
        assert!(rename(
            &mut list,
            "c",
            Kind::Request,
            "GET _cat/nodes",
            " nodes "
        ));
        assert_eq!(list[0].name, "nodes");
        assert!(!rename(
            &mut list,
            "c",
            Kind::Request,
            "GET _cat/nodes",
            " "
        ));
        assert!(!rename(
            &mut list,
            "x",
            Kind::Request,
            "GET _cat/nodes",
            "n"
        ));
        assert_eq!(name_of("\n  GET a/_search  \n{}"), "GET a/_search");
    }

    #[test]
    fn a_console_is_one_item_per_connection_and_name_and_follows_the_text() {
        let mut list = Vec::new();
        assert!(set_console(&mut list, "c", AUTO_CONSOLE, "GET a", 1));
        assert!(
            !set_console(&mut list, "c", AUTO_CONSOLE, "GET a", 2),
            "unchanged"
        );
        assert!(set_console(&mut list, "c", AUTO_CONSOLE, "GET b", 3));
        assert!(set_console(&mut list, "d", AUTO_CONSOLE, "GET b", 3));
        assert!(!set_console(
            &mut list,
            "c",
            AUTO_CONSOLE,
            "PUT _security/user/u",
            4
        ));
        assert!(!set_console(&mut list, "c", AUTO_CONSOLE, "  ", 4));
        assert!(set_console(&mut list, "c", "users", "GET users", 5));
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].text, "GET b");
    }
}
