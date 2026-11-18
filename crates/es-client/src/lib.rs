//! HTTP client for Elasticsearch: raw method/path/body requests, auth, timeout, cancellation.
//!
//! The client owns a small tokio runtime so callers may await from any executor. Every call returns
//! a [`Task`]; dropping it or calling [`Task::abort`] cancels the in-flight request.

pub mod browse;
pub mod catalog;
pub mod connection;
pub mod history;
mod hits;
mod json_tree;
mod meta;
pub mod saved;
mod store;
pub use connection::{AuthKind, Connection, Keychain, Secrets};

pub use hits::{CellKind, Hits, parse_hits};
pub use json_tree::{Node, json_tree};
pub use meta::ClusterMeta;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use es_dsl_core::{Version, parse_version};
use reqwest::Method;
use tokio::runtime::{Builder, Runtime};
use tokio::task::JoinHandle;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("unsupported HTTP method {0:?}")]
    Method(String),
    #[error("request failed: {0}")]
    Http(reqwest::Error),
    #[error("unexpected response: {0}")]
    Response(String),
    #[error("request cancelled")]
    Cancelled,
}

impl From<reqwest::Error> for Error {
    /// The URL is dropped from the message: it is the only place a mistyped credential could show up.
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.without_url())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Default)]
pub enum Auth {
    #[default]
    None,
    Basic {
        user: String,
        password: String,
    },
    /// The base64 `id:key` value Elasticsearch returns when an API key is created.
    ApiKey(String),
    /// An OAuth2 / service-account token sent as `Authorization: Bearer`.
    Bearer(String),
}

/// Secrets stay out of logs and panic messages.
impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Basic { user, .. } => {
                write!(f, "Basic {{ user: {user:?}, password: <redacted> }}")
            }
            Self::ApiKey(_) => f.write_str("ApiKey(<redacted>)"),
            Self::Bearer(_) => f.write_str("Bearer(<redacted>)"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// e.g. `https://localhost:9200`; must not contain credentials.
    pub base_url: String,
    pub auth: Auth,
    /// Whole-request timeout.
    pub timeout: Duration,
    /// Extra trusted root certificate (PEM), for clusters with a private CA.
    pub ca_pem: Option<Vec<u8>>,
    /// Accept any server certificate. Only for clusters whose certificate cannot be trusted another way.
    pub insecure: bool,
}

impl Config {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            auth: Auth::None,
            timeout: Duration::from_secs(30),
            ca_pem: None,
            insecure: false,
        }
    }
}

/// A console request: `path` includes the query string, as typed on the request line.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Option<String>,
}

impl Request {
    /// The request block under `cursor` (a byte offset), as the console would send it.
    /// Comment lines (`#`, `//`) are dropped from the body; `None` when there is no request line
    /// or its path is still empty.
    pub fn at(text: &str, cursor: usize) -> Option<Self> {
        let block = es_dsl_core::block_at(text, cursor)?;
        if block.path.is_empty() {
            return None;
        }
        let path = match &block.query {
            Some(q) => format!("{}?{q}", block.path),
            None => block.path.clone(),
        };
        let body: Vec<&str> = text[block.body.clone()]
            .lines()
            .filter(|l| {
                let l = l.trim_start();
                !l.starts_with('#') && !l.starts_with("//")
            })
            .collect();
        let body = body.join("\n");
        let body = body.trim();
        Some(Self {
            method: block.method,
            path,
            body: (!body.is_empty()).then(|| format!("{body}\n")),
        })
    }
}

/// Non-2xx statuses are not errors: Elasticsearch explains failures in the body, which the console shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// shortcut: the whole body is buffered in memory; stream or cap it before large results matter.
    pub body: String,
}

/// An in-flight call. Awaiting yields the result; dropping it cancels the request.
pub struct Task<T>(JoinHandle<Result<T>>);

impl<T> Task<T> {
    pub fn abort(&self) {
        self.0.abort();
    }
}

impl<T> Future for Task<T> {
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|r| r.map_err(|_| Error::Cancelled)?)
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct Client {
    http: reqwest::Client,
    base: String,
    auth: Auth,
    rt: Option<Runtime>,
}

impl Client {
    pub fn new(config: Config) -> Result<Self> {
        let base = config.base_url.trim_end_matches('/').to_string();
        let url = reqwest::Url::parse(&base).map_err(|e| Error::Config(e.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return Err(Error::Config("base_url must be an http(s) URL".into()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::Config(
                "credentials do not belong in base_url".into(),
            ));
        }
        let mut http = reqwest::Client::builder()
            .timeout(config.timeout)
            .danger_accept_invalid_certs(config.insecure);
        if let Some(pem) = &config.ca_pem {
            let cert = reqwest::Certificate::from_pem(pem)?;
            http = http.add_root_certificate(cert);
        }
        let rt = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| Error::Config(e.to_string()))?;
        Ok(Self {
            http: http.build()?,
            base,
            auth: config.auth,
            rt: Some(rt),
        })
    }

    pub fn send(&self, req: Request) -> Task<Response> {
        let http = self.http.clone();
        let auth = self.auth.clone();
        let url = format!("{}/{}", self.base, req.path.trim_start_matches('/'));
        self.spawn(async move { execute(&http, &auth, &url, req).await })
    }

    /// The cluster's stack version from `GET /`; a `-SNAPSHOT` or similar suffix is ignored.
    pub fn version(&self) -> Task<Version> {
        let http = self.http.clone();
        let auth = self.auth.clone();
        let url = format!("{}/", self.base);
        let req = Request {
            method: "GET".into(),
            path: String::new(),
            body: None,
        };
        self.spawn(async move {
            let resp = execute(&http, &auth, &url, req).await?;
            if resp.status != 200 {
                return Err(Error::Response(format!("GET / returned {}", resp.status)));
            }
            let json: serde_json::Value =
                serde_json::from_str(&resp.body).map_err(|e| Error::Response(e.to_string()))?;
            let number = json["version"]["number"]
                .as_str()
                .ok_or_else(|| Error::Response("no version.number in the response".into()))?;
            let plain = number.split('-').next().unwrap_or(number);
            parse_version(plain).ok_or_else(|| Error::Response(format!("bad version {number:?}")))
        })
    }

    fn spawn<T: Send + 'static>(
        &self,
        fut: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Task<T> {
        // `rt` is only taken in `drop`, so it is always present here.
        match &self.rt {
            Some(rt) => Task(rt.spawn(fut)),
            None => unreachable!("runtime is only released on drop"),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // A plain drop would panic when the client is released from inside an async context.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// NDJSON endpoints take `application/x-ndjson`; everything else is JSON.
/// shortcut: matched by path segment, not by the spec's endpoint list; add a name here if another NDJSON endpoint is needed.
fn content_type(path: &str) -> &'static str {
    let path = path.split('?').next().unwrap_or(path);
    let ndjson = path.split('/').any(|s| matches!(s, "_bulk" | "_msearch"));
    if ndjson {
        "application/x-ndjson"
    } else {
        "application/json"
    }
}

async fn execute(http: &reqwest::Client, auth: &Auth, url: &str, req: Request) -> Result<Response> {
    let method = Method::from_bytes(req.method.to_ascii_uppercase().as_bytes())
        .map_err(|_| Error::Method(req.method.clone()))?;
    let mut rb = http.request(method, url);
    rb = match auth {
        Auth::None => rb,
        Auth::Basic { user, password } => rb.basic_auth(user, Some(password)),
        Auth::ApiKey(key) => rb.header("Authorization", format!("ApiKey {key}")),
        Auth::Bearer(token) => rb.bearer_auth(token),
    };
    if let Some(body) = req.body {
        rb = rb
            .header("Content-Type", content_type(&req.path))
            .body(body);
    }
    let resp = rb.send().await?;
    let status = resp.status().as_u16();
    Ok(Response {
        status,
        body: resp.text().await?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[test]
    fn ndjson_endpoints_get_the_ndjson_content_type() {
        assert_eq!(content_type("_bulk?refresh=true"), "application/x-ndjson");
        assert_eq!(content_type("idx/_bulk"), "application/x-ndjson");
        assert_eq!(content_type("_msearch/template"), "application/x-ndjson");
        assert_eq!(content_type("idx/_search"), "application/json");
    }

    #[test]
    fn request_at_takes_the_block_under_the_cursor() {
        let text = "# note\nGET a/_search?size=1\n// skip\n{\"q\": 1}\n\nDELETE b\n";
        let first = Request::at(text, 10).unwrap();
        assert_eq!(first.method, "GET");
        assert_eq!(first.path, "a/_search?size=1");
        assert_eq!(first.body.as_deref(), Some("{\"q\": 1}\n"));
        let second = Request::at(text, text.len()).unwrap();
        assert_eq!((second.method.as_str(), second.body), ("DELETE", None));
        // A console opened from the list starts with the caret at offset 0; a leading blank line
        // must not make the request under it unreachable.
        let opened = "\nGET a/_search\n{\"size\": 1}\n";
        let first = Request::at(opened, 0).unwrap();
        assert_eq!(
            (first.method.as_str(), first.path.as_str()),
            ("GET", "a/_search")
        );
        assert_eq!(first.body.as_deref(), Some("{\"size\": 1}\n"));
        assert!(Request::at("GET\n", 0).is_none());
        assert!(Request::at("", 0).is_none());
    }

    fn client(server: &MockServer, auth: Auth) -> Client {
        let mut cfg = Config::new(server.base_url());
        cfg.auth = auth;
        cfg.timeout = Duration::from_millis(500);
        Client::new(cfg).unwrap()
    }

    #[tokio::test]
    async fn sends_raw_requests_with_auth() {
        let server = MockServer::start_async().await;
        let m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/idx/_search")
                    .query_param("pretty", "true")
                    .header("authorization", "ApiKey abc")
                    .header("content-type", "application/json")
                    .body("{\"size\":1}");
                then.status(200).body("{\"hits\":[]}");
            })
            .await;
        let c = client(&server, Auth::ApiKey("abc".into()));
        let resp = c
            .send(Request {
                method: "post".into(),
                path: "/idx/_search?pretty=true".into(),
                body: Some("{\"size\":1}".into()),
            })
            .await
            .unwrap();
        m.assert_async().await;
        assert_eq!(
            resp,
            Response {
                status: 200,
                body: "{\"hits\":[]}".into()
            }
        );
    }

    #[tokio::test]
    async fn bearer_token_is_sent_as_a_bearer_header() {
        let server = MockServer::start_async().await;
        let m = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/x")
                    .header("authorization", "Bearer tok");
                then.status(200).body("{}");
            })
            .await;
        let req = Request {
            method: "GET".into(),
            path: "x".into(),
            body: None,
        };
        client(&server, Auth::Bearer("tok".into()))
            .send(req)
            .await
            .unwrap();
        m.assert_async().await;
    }

    #[tokio::test]
    async fn basic_auth_and_error_status_are_passed_through() {
        let server = MockServer::start_async().await;
        // base64("u:p")
        server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/x")
                    .header("authorization", "Basic dTpw");
                then.status(404).body("{\"error\":\"nope\"}");
            })
            .await;
        let auth = Auth::Basic {
            user: "u".into(),
            password: "p".into(),
        };
        let resp = client(&server, auth)
            .send(Request {
                method: "GET".into(),
                path: "x".into(),
                body: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn reads_the_version_and_ignores_suffixes() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/");
                then.status(200)
                    .body("{\"version\":{\"number\":\"8.11.2-SNAPSHOT\"}}");
            })
            .await;
        assert_eq!(
            client(&server, Auth::None).version().await.unwrap(),
            [8, 11, 2]
        );
    }

    #[tokio::test]
    async fn version_rejects_garbage() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/");
                then.status(200).body("{\"tagline\":\"hi\"}");
            })
            .await;
        let err = client(&server, Auth::None).version().await.unwrap_err();
        assert!(matches!(err, Error::Response(_)), "{err}");
    }

    #[tokio::test]
    async fn times_out_and_cancels() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/slow");
                then.status(200).delay(Duration::from_secs(5));
            })
            .await;
        let c = client(&server, Auth::None);
        let slow = || Request {
            method: "GET".into(),
            path: "slow".into(),
            body: None,
        };
        assert!(matches!(c.send(slow()).await, Err(Error::Http(e)) if e.is_timeout()));
        let task = c.send(slow());
        task.abort();
        assert!(matches!(task.await, Err(Error::Cancelled)));
    }

    #[test]
    fn validates_config_and_hides_secrets() {
        for bad in ["not a url", "ftp://h", "http://u:p@h:9200"] {
            assert!(
                matches!(Client::new(Config::new(bad)), Err(Error::Config(_))),
                "{bad}"
            );
        }
        let shown = format!("{:?}", Auth::ApiKey("topsecret".into()));
        let basic = format!(
            "{:?}",
            Auth::Basic {
                user: "u".into(),
                password: "topsecret".into()
            }
        );
        let bearer = format!("{:?}", Auth::Bearer("topsecret".into()));
        assert!(!shown.contains("topsecret") && !basic.contains("topsecret"));
        assert!(!bearer.contains("topsecret"));
    }

    #[test]
    fn unsupported_method_is_reported() {
        let c = Client::new(Config::new("http://127.0.0.1:1")).unwrap();
        let rt = Builder::new_current_thread().build().unwrap();
        let err = rt
            .block_on(c.send(Request {
                method: "NOT A METHOD".into(),
                path: "".into(),
                body: None,
            }))
            .unwrap_err();
        assert!(matches!(err, Error::Method(_)), "{err}");
    }
}
