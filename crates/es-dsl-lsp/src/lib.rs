//! LSP adapter over `es-dsl-core`: full-text document sync plus `textDocument/completion`.

use std::collections::HashMap;

use anyhow::Result;
use es_dsl_core::{
    Candidate, Fields, Index, Kind, NoFields, Severity, Version, candidates_with, diagnostics,
    hover,
};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, CompletionParams, CompletionResponse,
    CompletionTextEdit, Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, Hover, HoverContents, HoverParams,
    HoverProviderCapability, LogMessageParams, MarkupContent, MarkupKind, MessageType, Position,
    PublishDiagnosticsParams, Range, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextEdit, Uri,
    notification::{self, Notification as _},
    request::{self, Request as _},
};

/// Keyed by the URI string: `Uri` has interior mutability, which clippy rejects as a map key.
type Docs = HashMap<String, String>;

fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec!["\"".into()]),
            ..CompletionOptions::default()
        }),
        ..ServerCapabilities::default()
    }
}

/// Runs the server on `conn` until `shutdown` + `exit` (or the peer disconnects). `version` is the
/// cluster's stack version; candidates newer than it are hidden.
pub fn serve(conn: &Connection, index: &Index, version: Option<Version>) -> Result<()> {
    serve_with(conn, index, version, &NoFields)
}

/// Like [`serve`], with field names for the target index supplied by `fields`.
pub fn serve_with(
    conn: &Connection,
    index: &Index,
    version: Option<Version>,
    fields: &dyn Fields,
) -> Result<()> {
    conn.initialize(serde_json::to_value(capabilities())?)?;
    let mut docs = Docs::new();
    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if conn.handle_shutdown(&req)? {
                    return Ok(());
                }
                conn.sender
                    .send(on_request(conn, index, version, fields, &docs, req).into())?;
            }
            Message::Notification(n) => on_notification(conn, index, version, &mut docs, n),
            Message::Response(_) => {}
        }
    }
    Ok(())
}

fn on_notification(
    conn: &Connection,
    index: &Index,
    version: Option<Version>,
    docs: &mut Docs,
    n: Notification,
) {
    let mut changed: Option<Uri> = None;
    match n.method.as_str() {
        notification::DidOpenTextDocument::METHOD => {
            if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(n.params) {
                docs.insert(p.text_document.uri.to_string(), p.text_document.text);
                changed = Some(p.text_document.uri);
            }
        }
        notification::DidChangeTextDocument::METHOD => {
            // Sync kind is FULL, so the last change carries the whole text.
            if let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(n.params)
                && let Some(change) = p.content_changes.into_iter().next_back()
            {
                docs.insert(p.text_document.uri.to_string(), change.text);
                changed = Some(p.text_document.uri);
            }
        }
        notification::DidCloseTextDocument::METHOD => {
            if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(n.params) {
                docs.remove(p.text_document.uri.as_str());
                publish(conn, p.text_document.uri, Vec::new());
            }
        }
        _ => {}
    }
    if let Some(uri) = changed
        && let Some(text) = docs.get(uri.as_str())
    {
        let found = diagnostics(index, text, version)
            .into_iter()
            .map(|d| Diagnostic {
                range: Range::new(position(text, d.range.start), position(text, d.range.end)),
                severity: Some(match d.severity {
                    Severity::Error => DiagnosticSeverity::ERROR,
                    Severity::Warning => DiagnosticSeverity::WARNING,
                }),
                message: d.message,
                source: Some("es-dsl".into()),
                ..Diagnostic::default()
            })
            .collect();
        publish(conn, uri, found);
    }
}

fn publish(conn: &Connection, uri: Uri, diagnostics: Vec<Diagnostic>) {
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version: None,
    };
    // A closed channel only means the client is gone; the main loop notices on its next read.
    let _ = conn
        .sender
        .send(Notification::new(notification::PublishDiagnostics::METHOD.into(), params).into());
}

fn on_request(
    conn: &Connection,
    index: &Index,
    version: Option<Version>,
    fields: &dyn Fields,
    docs: &Docs,
    req: Request,
) -> Response {
    if req.method == request::HoverRequest::METHOD {
        return on_hover(index, version, docs, req);
    }
    if req.method != request::Completion::METHOD {
        return Response::new_err(
            req.id,
            ErrorCode::MethodNotFound as i32,
            format!("unsupported method {}", req.method),
        );
    }
    let params: CompletionParams = match serde_json::from_value(req.params) {
        Ok(p) => p,
        Err(e) => return Response::new_err(req.id, ErrorCode::InvalidParams as i32, e.to_string()),
    };
    let pos = params.text_document_position;
    let items = match docs.get(pos.text_document.uri.as_str()) {
        None => {
            log(conn, "completion: document is not open");
            Vec::new()
        }
        Some(text) => {
            let cursor = offset(text, pos.position);
            match candidates_with(index, text, cursor, version, fields) {
                Ok(found) => found.into_iter().map(|c| item(c, text, cursor)).collect(),
                Err(why) => {
                    log(conn, &format!("completion: no candidates ({why:?})"));
                    Vec::new()
                }
            }
        }
    };
    Response::new_ok(req.id, CompletionResponse::Array(items))
}

fn on_hover(index: &Index, version: Option<Version>, docs: &Docs, req: Request) -> Response {
    let params: HoverParams = match serde_json::from_value(req.params) {
        Ok(p) => p,
        Err(e) => return Response::new_err(req.id, ErrorCode::InvalidParams as i32, e.to_string()),
    };
    let pos = params.text_document_position_params;
    let found = docs.get(pos.text_document.uri.as_str()).and_then(|text| {
        let h = hover(index, text, offset(text, pos.position), version)?;
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: h.markdown,
            }),
            range: Some(Range::new(
                position(text, h.range.start),
                position(text, h.range.end),
            )),
        })
    });
    Response::new_ok(req.id, found)
}

/// Empty results are explained through `window/logMessage` (LSP has no field for it).
fn item(c: Candidate, text: &str, cursor: usize) -> CompletionItem {
    let text_edit = body_edit(&c, text, cursor).map(|(range, new_text)| {
        CompletionTextEdit::Edit(TextEdit::new(
            Range::new(position(text, range.start), position(text, range.end)),
            new_text,
        ))
    });
    CompletionItem {
        text_edit,
        label: c.label,
        kind: Some(match c.kind {
            Kind::Method => CompletionItemKind::KEYWORD,
            Kind::Path => CompletionItemKind::FOLDER,
            Kind::Index => CompletionItemKind::MODULE,
            Kind::Param => CompletionItemKind::VARIABLE,
            Kind::Key => CompletionItemKind::PROPERTY,
            Kind::Value => CompletionItemKind::ENUM_MEMBER,
            Kind::Field => CompletionItemKind::FIELD,
        }),
        detail: c.detail,
        ..CompletionItem::default()
    }
}

/// The exact edit for a JSON key or value: it swallows what the user already typed, quotes included,
/// and writes the whole quoted token, so the client never has to guess a word boundary.
/// Request-line candidates replace the path segment typed so far.
fn body_edit(c: &Candidate, text: &str, cursor: usize) -> Option<(std::ops::Range<usize>, String)> {
    if matches!(
        c.kind,
        Kind::Method | Kind::Index | Kind::Path | Kind::Param
    ) {
        // Request line: replace the segment typed so far; the client's word range stops at `_` and `-`.
        let before = &text[..cursor];
        let start = before
            .char_indices()
            .rfind(|(_, ch)| ch.is_whitespace() || "/,?&".contains(*ch))
            .map_or(0, |(i, ch)| i + ch.len_utf8());
        return Some((start..cursor, c.label.clone()));
    }
    let before = &text[..cursor];
    let line = &before[before.rfind('\n').map_or(0, |i| i + 1)..];
    let mut open = None;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        match ch {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => open = if open.is_some() { None } else { Some(i) },
            _ => {}
        }
    }
    let line_start = before.len() - line.len();
    let (start, end, quoted) = if let Some(q) = open {
        // Inside a string: `label` has no quotes here; replace from the opening quote through any closing one.
        (
            line_start + q,
            cursor + usize::from(text[cursor..].starts_with('"')),
            true,
        )
    } else {
        let typed = line
            .rfind(|ch: char| !(ch.is_alphanumeric() || "_.-".contains(ch)))
            .map_or(0, |i| {
                i + line[i..].chars().next().map_or(1, char::len_utf8)
            });
        // A key or field name is always a string; a bare literal (true, 12) or an already quoted value is not.
        let quoted = c.kind != Kind::Value && !c.label.starts_with('"');
        (line_start + typed, cursor, quoted)
    };
    let mut new_text = if quoted {
        format!("\"{}\"", c.label.trim_matches('"'))
    } else {
        c.label.clone()
    };
    // A key is followed by its value, unless one is already there or this is a value position.
    let is_value = text[..start].trim_end().ends_with(':');
    let has_colon = text[end..].trim_start().starts_with(':');
    if matches!(c.kind, Kind::Key | Kind::Field) && !is_value && !has_colon {
        new_text.push_str(": ");
    }
    Some((start..end, new_text))
}

fn log(conn: &Connection, message: &str) {
    let params = LogMessageParams {
        typ: MessageType::LOG,
        message: message.into(),
    };
    // A closed channel only means the client is gone; the main loop notices on its next read.
    let _ = conn
        .sender
        .send(Notification::new(notification::LogMessage::METHOD.into(), params).into());
}

/// Converts an LSP position (line, UTF-16 column) to a byte offset, clamping past-the-end positions.
fn offset(text: &str, pos: Position) -> usize {
    let mut start = 0;
    for _ in 0..pos.line {
        match text[start..].find('\n') {
            Some(i) => start += i + 1,
            None => return text.len(),
        }
    }
    let mut units = 0;
    for (i, c) in text[start..].char_indices() {
        if units >= pos.character as usize || c == '\n' {
            return start + i;
        }
        units += c.len_utf16();
    }
    text.len()
}

/// Converts a byte offset to an LSP position (line, UTF-16 column); the inverse of [`offset`].
fn position(text: &str, at: usize) -> Position {
    let head = &text[..at.min(text.len())];
    let line = head.matches('\n').count();
    let start = head.rfind('\n').map_or(0, |i| i + 1);
    let character = head[start..].chars().map(char::len_utf16).sum::<usize>();
    Position::new(line as u32, character as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use es_dsl_core::{Endpoint, Prop, TypeRef, Url};
    use lsp_server::RequestId;
    use serde_json::{Value, json};

    #[test]
    fn edits_quote_and_replace_what_was_typed() {
        let cand = |label: &str, kind| Candidate {
            label: label.into(),
            kind,
            detail: None,
        };
        let key = |label| cand(label, Kind::Key);
        let apply = |text: &str, c: &Candidate| {
            let cursor = text.find('|').unwrap();
            let text = text.replace('|', "");
            let (r, new) = body_edit(c, &text, cursor).unwrap();
            format!("{}{}{}", &text[..r.start], new, &text[r.end..])
        };
        assert_eq!(
            apply("GET tb1_re|", &cand("tb1_recommend_index", Kind::Index)),
            "GET tb1_recommend_index"
        );
        assert_eq!(
            apply("GET a,tb1_re|", &cand("tb1_x", Kind::Index)),
            "GET a,tb1_x"
        );
        assert_eq!(
            apply("GET idx/_se|", &cand("_search", Kind::Path)),
            "GET idx/_search"
        );
        assert_eq!(
            apply("GET idx/_search?pre|", &cand("pretty", Kind::Param)),
            "GET idx/_search?pretty"
        );
        assert_eq!(apply("{\n  |\n}", &key("query")), "{\n  \"query\": \n}");
        assert_eq!(apply("{\"qu|\"}", &key("query")), "{\"query\": }");
        assert_eq!(apply("{\"qu|}", &key("query")), "{\"query\": }");
        assert_eq!(
            apply("{\"a\": 1, que|", &key("query")),
            "{\"a\": 1, \"query\": "
        );
        // No second colon when one follows, and none in a value position.
        assert_eq!(apply("{\"qu|\": {}}", &key("query")), "{\"query\": {}}");
        let field = cand("title", Kind::Field);
        assert_eq!(apply("{\"a\": \"ti|\"}", &field), "{\"a\": \"title\"}");
        let flag = cand("true", Kind::Value);
        assert_eq!(apply("{\"a\": tr|", &flag), "{\"a\": true");
    }

    #[test]
    fn offset_table() {
        let text = "ab\n中😀x\r\nz";
        let cases = [
            ((0, 1), 1),
            ((0, 99), 2),
            ((1, 1), 3 + 3),
            // the emoji is two UTF-16 units: column 2 is inside it (invalid) and rounds forward, never splitting the char
            ((1, 2), 3 + 3 + 4),
            ((1, 3), 3 + 3 + 4),
            ((2, 1), text.len()),
            ((9, 0), text.len()),
        ];
        for ((line, character), want) in cases {
            assert_eq!(
                offset(text, Position { line, character }),
                want,
                "{line}:{character}"
            );
        }
    }

    fn index() -> Index {
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

    /// Drives a full session over an in-memory connection.
    #[test]
    fn completes_over_the_wire() {
        let (server, client) = Connection::memory();
        let idx = index();
        let handle = std::thread::spawn(move || serve(&server, &idx, None));

        let send = |m: Message| client.sender.send(m).unwrap();
        let req = |id: i32, method: &str, params: Value| {
            Message::Request(Request::new(RequestId::from(id), method.into(), params))
        };
        let notify = |method: &str, params: Value| {
            Message::Notification(Notification::new(method.into(), params))
        };
        let recv_response = |id: i32| loop {
            match client.receiver.recv().unwrap() {
                Message::Response(r) if r.id == RequestId::from(id) => return r,
                _ => {} // logMessage notifications
            }
        };

        send(req(1, "initialize", json!({"capabilities": {}})));
        let init = recv_response(1);
        assert_eq!(
            init.response_result.unwrap()["capabilities"]["completionProvider"]["triggerCharacters"],
            json!(["\""])
        );
        send(notify("initialized", json!({})));

        let uri = "file:///t.es";
        let text = "POST idx/_search\n{\"q";
        send(notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": uri, "languageId": "es", "version": 1, "text": "POST idx/_search\n{"}}),
        ));
        send(notify(
            "textDocument/didChange",
            json!({"textDocument": {"uri": uri, "version": 2}, "contentChanges": [{"text": text}]}),
        ));
        let complete_at = |id: i32, uri: &str| {
            req(
                id,
                "textDocument/completion",
                json!({"textDocument": {"uri": uri}, "position": {"line": 1, "character": 3}}),
            )
        };
        send(complete_at(2, uri));
        let items = recv_response(2).response_result.unwrap();
        assert_eq!(items[0]["label"], "query");
        assert_eq!(items.as_array().unwrap().len(), 1);

        send(complete_at(3, "file:///unknown.es"));
        assert_eq!(recv_response(3).response_result.unwrap(), json!([]));

        send(req(4, "textDocument/hover", json!({})));
        assert!(recv_response(4).response_result.is_err());

        let uri2 = "file:///d.es";
        send(notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": uri2, "languageId": "es", "version": 1,
                "text": "POST idx/_search\n{\"nope\":1,\"query\":{}}"}}),
        ));
        let published = loop {
            match client.receiver.recv().unwrap() {
                Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
                    break n.params;
                }
                _ => {}
            }
        };
        assert_eq!(published["uri"], uri2);
        assert_eq!(
            published["diagnostics"][0]["message"],
            "Unknown property `nope`"
        );
        assert_eq!(
            published["diagnostics"][0]["range"],
            json!({"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 7}})
        );
        send(req(
            6,
            "textDocument/hover",
            json!({"textDocument": {"uri": uri2}, "position": {"line": 1, "character": 12}}),
        ));
        let hover = recv_response(6).response_result.unwrap();
        assert_eq!(hover["contents"]["value"], "**query**");

        send(req(5, "shutdown", Value::Null));
        recv_response(5);
        send(notify("exit", Value::Null));
        handle.join().unwrap().unwrap();
    }
}
