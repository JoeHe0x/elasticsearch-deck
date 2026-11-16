//! In-process client for `es-dsl-lsp`: the same server code the standalone binary runs,
//! connected through `lsp_server::Connection::memory()` instead of stdio.

use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use es_dsl_core::{Fields, Index, Version};
use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{
    CompletionItem, CompletionResponse, Diagnostic, Hover, Position, PublishDiagnosticsParams,
};
use serde_json::{Value, json};

const URI: &str = "file:///console.es";

struct Session {
    conn: Connection,
    next_id: i32,
    version: i32,
    /// The server's latest `publishDiagnostics`, picked up while waiting for a response.
    diagnostics: Vec<Diagnostic>,
}

pub struct LspClient(Mutex<Session>);

impl LspClient {
    /// Starts the server on a background thread and completes the initialize handshake.
    pub fn start(
        index: Index,
        version: Option<Version>,
        fields: impl Fields + Send + 'static,
    ) -> Result<Arc<Self>> {
        let (server, client) = Connection::memory();
        // The server ends when the client side is dropped; its result carries nothing to report then.
        std::thread::spawn(move || es_dsl_lsp::serve_with(&server, &index, version, &fields));
        let mut s = Session {
            conn: client,
            next_id: 0,
            version: 0,
            diagnostics: Vec::new(),
        };
        s.request("initialize", json!({"capabilities": {}}))?;
        s.notify("initialized", json!({}))?;
        Ok(Arc::new(Self(Mutex::new(s))))
    }

    /// Syncs the whole document, then requests completion at `pos`. Blocks until the server answers,
    /// so call it from a background task.
    pub fn complete(&self, text: String, pos: Position) -> Result<Vec<CompletionItem>> {
        let mut s = self.0.lock().map_err(|_| anyhow!("lsp session poisoned"))?;
        s.sync(&text)?;
        let result = s.request(
            "textDocument/completion",
            json!({"textDocument": {"uri": URI}, "position": pos}),
        )?;
        match serde_json::from_value::<Option<CompletionResponse>>(result)? {
            Some(CompletionResponse::Array(items)) => Ok(items),
            Some(CompletionResponse::List(list)) => Ok(list.items),
            None => Ok(Vec::new()),
        }
    }

    /// Syncs the document, then asks for the hover at `pos`. Blocks; call it from a background task.
    pub fn hover(&self, text: String, pos: Position) -> Result<Option<Hover>> {
        let mut s = self.0.lock().map_err(|_| anyhow!("lsp session poisoned"))?;
        s.sync(&text)?;
        s.hover_at(pos)
    }

    /// Syncs the document and returns the diagnostics the server published for it. Blocks.
    pub fn diagnose(&self, text: String) -> Result<Vec<Diagnostic>> {
        let mut s = self.0.lock().map_err(|_| anyhow!("lsp session poisoned"))?;
        s.sync(&text)?;
        // The server publishes before it answers any later request, so this round trip is a barrier.
        s.hover_at(Position::new(0, 0))?;
        Ok(std::mem::take(&mut s.diagnostics))
    }
}

impl Session {
    fn sync(&mut self, text: &str) -> Result<()> {
        self.version += 1;
        if self.version == 1 {
            let doc = json!({"uri": URI, "languageId": "es", "version": 1, "text": text});
            self.notify("textDocument/didOpen", json!({ "textDocument": doc }))
        } else {
            let doc = json!({"uri": URI, "version": self.version});
            self.notify(
                "textDocument/didChange",
                json!({"textDocument": doc, "contentChanges": [{"text": text}]}),
            )
        }
    }

    fn hover_at(&mut self, pos: Position) -> Result<Option<Hover>> {
        let result = self.request(
            "textDocument/hover",
            json!({"textDocument": {"uri": URI}, "position": pos}),
        )?;
        Ok(serde_json::from_value(result)?)
    }

    fn notify(&self, method: &str, params: Value) -> Result<()> {
        let n = Notification::new(method.into(), params);
        self.conn.sender.send(n.into())?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.conn
            .sender
            .send(Request::new(id.clone(), method.into(), params).into())?;
        loop {
            match self.conn.receiver.recv()? {
                Message::Response(r) if r.id == id => {
                    return match r.response_result {
                        Ok(v) => Ok(v),
                        Err(e) => bail!("{method} failed: {}", e.message),
                    };
                }
                Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
                    if let Ok(p) = serde_json::from_value::<PublishDiagnosticsParams>(n.params) {
                        self.diagnostics = p.diagnostics;
                    }
                }
                // Anything else (e.g. `window/logMessage`) is skipped.
                _ => {}
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn sample_index() -> Index {
    use es_dsl_core::{Endpoint, Prop, TypeRef, Url};
    Index {
        endpoints: vec![Endpoint {
            name: "search".into(),
            urls: vec![Url {
                path: "/{index}/_search".into(),
                methods: vec!["POST".into()],
            }],
            body: vec![Prop::new("query", TypeRef::Other)],
            query: Vec::new(),
            since: None,
            ..Endpoint::default()
        }],
        ..Index::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_through_the_in_process_server() {
        let client = LspClient::start(sample_index(), None, es_dsl_core::NoFields).unwrap();
        let at = Position {
            line: 1,
            character: 3,
        };
        // First call opens the document, the second goes through didChange.
        for text in ["POST i/_search\n{\"q", "POST j/_search\n{\"q"] {
            let items = client.complete(text.into(), at).unwrap();
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].label, "query");
        }
        assert!(client.complete("{".into(), at).unwrap().is_empty());
    }
}
