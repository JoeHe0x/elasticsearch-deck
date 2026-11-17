//! Saved cluster connections. The list lives in a JSON file without secrets; passwords and API
//! keys live only in the OS keychain, keyed by connection name.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Auth, Config, Error, Result};

const SERVICE: &str = "elasticsearch-deck";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthKind {
    #[default]
    None,
    Basic,
    ApiKey,
    Bearer,
}

/// Everything about a connection except its secret.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Connection {
    /// Unique; also the keychain account name.
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub auth: AuthKind,
    /// Basic auth only.
    #[serde(default)]
    pub user: String,
    /// Path of an extra trusted root certificate (PEM); empty for none.
    #[serde(default)]
    pub ca_file: String,
    /// Accept any server certificate.
    #[serde(default)]
    pub insecure: bool,
    /// Whole-request timeout in seconds; 0 keeps the default.
    #[serde(default)]
    pub timeout_secs: u64,
}

/// The token without a pasted `Bearer ` prefix, so both forms work; the header adds it back.
fn bare_token(token: &str) -> &str {
    let token = token.trim();
    match token.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("bearer ") => token[7..].trim_start(),
        _ => token,
    }
}

impl Connection {
    /// Builds a client config; `secret` is the password, API key or bearer token, as the auth kind requires.
    pub fn config(&self, secret: Option<String>) -> Result<Config> {
        let mut cfg = Config::new(self.url.clone());
        cfg.auth = match (self.auth, secret) {
            (AuthKind::None, _) => Auth::None,
            (AuthKind::Basic, Some(password)) => Auth::Basic {
                user: self.user.clone(),
                password,
            },
            (AuthKind::ApiKey, Some(key)) => Auth::ApiKey(key),
            (AuthKind::Bearer, Some(token)) => Auth::Bearer(bare_token(&token).into()),
            (_, None) => return Err(Error::Config("the connection needs a secret".into())),
        };
        if !self.ca_file.is_empty() {
            let pem = std::fs::read(&self.ca_file)
                .map_err(|e| Error::Config(format!("cannot read the CA certificate: {e}")))?;
            cfg.ca_pem = Some(pem);
        }
        cfg.insecure = self.insecure;
        if self.timeout_secs > 0 {
            cfg.timeout = std::time::Duration::from_secs(self.timeout_secs);
        }
        Ok(cfg)
    }
}

/// Reads the saved connections; a missing file is an empty list, an unreadable one is moved aside and reported.
pub fn load(path: &Path) -> Result<Vec<Connection>> {
    Ok(crate::store::read_json(path)?.unwrap_or_default())
}

/// Replaces the saved list. Names must be unique, since they key the keychain entries.
pub fn save(path: &Path, conns: &[Connection]) -> Result<()> {
    let mut names: Vec<&str> = conns.iter().map(|c| c.name.as_str()).collect();
    names.sort_unstable();
    if names.windows(2).any(|w| w[0] == w[1]) || names.contains(&"") {
        return Err(Error::Config(
            "connection names must be unique and non-empty".into(),
        ));
    }
    let text = serde_json::to_string_pretty(conns).map_err(|e| Error::Config(e.to_string()))?;
    crate::store::write_atomic(path, &text)
}

/// Where secrets are kept; [`Keychain`] is the real one.
pub trait Secrets {
    fn get(&self, name: &str) -> Result<Option<String>>;
    fn set(&self, name: &str, secret: &str) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
}

/// The OS credential store (Keychain, Credential Manager, Secret Service).
pub struct Keychain;

fn entry(name: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, name).map_err(|e| Error::Config(format!("keychain: {e}")))
}

impl Secrets for Keychain {
    fn get(&self, name: &str) -> Result<Option<String>> {
        match entry(name)?.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Error::Config(format!("keychain: {e}"))),
        }
    }

    fn set(&self, name: &str, secret: &str) -> Result<()> {
        entry(name)?
            .set_password(secret)
            .map_err(|e| Error::Config(format!("keychain: {e}")))
    }

    fn delete(&self, name: &str) -> Result<()> {
        match entry(name)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(Error::Config(format!("keychain: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(name: &str) -> Connection {
        Connection {
            name: name.into(),
            url: "https://localhost:9200".into(),
            auth: AuthKind::Basic,
            user: "elastic".into(),
            ..Connection::default()
        }
    }

    #[test]
    fn file_round_trip_holds_no_secret() {
        let dir = std::env::temp_dir().join(format!("esdeck-conn-{}", std::process::id()));
        let path = dir.join("connections.json");
        assert!(load(&path).unwrap().is_empty());
        save(&path, &[conn("a"), conn("b")]).unwrap();
        assert_eq!(load(&path).unwrap(), [conn("a"), conn("b")]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"elastic\"") && !text.contains("password"));
        assert!(save(&path, &[conn("a"), conn("a")]).is_err());
        assert!(save(&path, &[conn("")]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn config_needs_the_secret_its_auth_kind_requires() {
        let c = conn("a");
        assert!(c.config(None).is_err());
        let cfg = c.config(Some("pw".into())).unwrap();
        assert!(matches!(cfg.auth, Auth::Basic { ref user, .. } if user == "elastic"));
        let bearer = Connection {
            auth: AuthKind::Bearer,
            ..conn("b")
        };
        assert!(matches!(
            bearer.config(Some("t".into())).unwrap().auth,
            Auth::Bearer(_)
        ));
        for pasted in ["t", " t ", "Bearer t", "bearer  t"] {
            assert!(matches!(
                bearer.config(Some(pasted.into())).unwrap().auth,
                Auth::Bearer(ref t) if t == "t"
            ));
        }
        let open = Connection {
            auth: AuthKind::None,
            ..c
        };
        assert!(matches!(open.config(None).unwrap().auth, Auth::None));
    }

    #[test]
    fn tls_options_reach_the_config() {
        let dir = std::env::temp_dir().join(format!("esdeck-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ca = dir.join("ca.pem");
        std::fs::write(&ca, b"pem").unwrap();
        let c = Connection {
            auth: AuthKind::None,
            ca_file: ca.to_string_lossy().into_owned(),
            insecure: true,
            timeout_secs: 5,
            ..conn("a")
        };
        let cfg = c.config(None).unwrap();
        assert_eq!(cfg.ca_pem.as_deref(), Some(&b"pem"[..]));
        assert!(cfg.insecure);
        assert_eq!(cfg.timeout, std::time::Duration::from_secs(5));
        let missing = Connection {
            ca_file: dir.join("none.pem").to_string_lossy().into_owned(),
            ..c
        };
        assert!(missing.config(None).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
