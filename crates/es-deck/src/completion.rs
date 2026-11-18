//! Bridges the GPUI Kit editor's completion and hover providers to the in-process LSP client.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use es_dsl_core::Field;

use anyhow::Result;
use gpui_kit::component::Rope;
use gpui_kit::component::input::{CompletionProvider, HoverProvider, RopeExt as _};
use gpui_kit::{App, AppContext as _, Task};
use lsp_types::{CompletionContext, CompletionItem, CompletionResponse, CompletionTriggerKind};

use crate::lsp_client::LspClient;

/// Marks the letters of the label that the typed text matches as a case-insensitive subsequence, so
/// `scr` lights up `_score`'s `s`, `c`, `r`. The kit's default only marks an exact prefix.
pub fn highlight_subsequence(query: &str, item: &CompletionItem) -> Vec<Range<usize>> {
    let mut wanted = query.chars().flat_map(char::to_lowercase).peekable();
    let mut out = Vec::new();
    for (at, ch) in item.label.char_indices() {
        let Some(next) = wanted.peek() else { break };
        if ch.to_lowercase().next() == Some(*next) {
            out.push(at..at + ch.len_utf8());
            wanted.next();
        }
    }
    // A partial match would mark letters the user did not type in order.
    if wanted.peek().is_some() {
        Vec::new()
    } else {
        out
    }
}

fn is_trigger_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '"')
}

pub struct LspCompletion(pub Arc<LspClient>);

impl CompletionProvider for LspCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _window: &mut gpui_kit::Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let pos = text.offset_to_position(offset);
        let text = text.to_string();
        // A bare opening quote is not a request yet: the menu would open with nothing typed and
        // its first item preselected, so the next Enter would accept that item instead of
        // breaking the line. Typing a character asks again, and so does ShowCompletions.
        if trigger.trigger_kind == CompletionTriggerKind::TRIGGER_CHARACTER
            && text
                .get(..offset)
                .is_some_and(|before| before.ends_with('"'))
        {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        }
        let client = self.0.clone();
        // The client blocks on the server's answer, so keep it off the UI thread.
        cx.background_spawn(
            async move { Ok(CompletionResponse::Array(client.complete(text, pos)?)) },
        )
    }

    // The server decides what is completable, so the only rule here is when to ask: after
    // identifier characters, never after structure (`{ [ , :`) or whitespace, where an
    // auto-opened menu would swallow Enter. A quote is reported as a trigger too, but the
    // provider declines it until a character is typed or ShowCompletions asks explicitly.
    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        new_text.chars().any(is_trigger_char)
    }
}

/// Completes field names in the Data Browser's filter box, with the mapping type as the detail.
pub struct FieldCompletion(pub Rc<dyn Fn() -> Arc<[Field]>>);

/// Start of the field-name word that ends at `offset`, or `None` when the caret is in a value
/// (after `:` or a comparison operator) or inside quotes.
fn field_word(before: &str) -> Option<usize> {
    let start = before
        .rfind(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | '@')))
        .map_or(0, |i| {
            i + before[i..].chars().next().map_or(1, char::len_utf8)
        });
    let prev = before[..start].trim_end().chars().next_back();
    let quoted = before.matches('"').count() % 2 == 1;
    (!quoted && !matches!(prev, Some(':' | '<' | '>' | '='))).then_some(start)
}

impl CompletionProvider for FieldCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut gpui_kit::Window,
        _cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let before = text.to_string();
        let before = before.get(..offset).unwrap_or_default();
        let Some(start) = field_word(before) else {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        };
        let typed = &before[start..];
        let range = lsp_types::Range::new(
            text.offset_to_position(start),
            text.offset_to_position(offset),
        );
        let items = (self.0)()
            .iter()
            .filter(|f| !f.nested)
            .map(|f| CompletionItem {
                label: f.name.clone(),
                detail: Some(f.ty.clone()),
                kind: Some(lsp_types::CompletionItemKind::FIELD),
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(
                    lsp_types::TextEdit::new(range, f.name.clone()),
                )),
                ..Default::default()
            })
            .filter(|i| typed.is_empty() || !highlight_subsequence(typed, i).is_empty())
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        new_text.chars().any(is_trigger_char)
    }
}

pub struct LspHover(pub Arc<LspClient>);

impl HoverProvider for LspHover {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut gpui_kit::Window,
        cx: &mut App,
    ) -> Task<Result<Option<lsp_types::Hover>>> {
        let pos = text.offset_to_position(offset);
        let text = text.to_string();
        let client = self.0.clone();
        cx.background_spawn(async move { client.hover(text, pos) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_names_are_offered_only_outside_values_and_quotes() {
        assert_eq!(
            field_word("Title : S* and IMDB_Rating > 7.5 and c"),
            Some(37)
        );
        assert_eq!(field_word(""), Some(0));
        assert_eq!(field_word("a:"), None);
        assert_eq!(field_word("a > 7"), None);
        assert_eq!(field_word("a : \"why c"), None);
        assert_eq!(field_word("(a:1 or "), Some(8));
    }

    #[test]
    fn structure_and_whitespace_do_not_trigger() {
        for t in ["{", "[", ",", ":", " ", "\n", ""] {
            assert!(!t.chars().any(is_trigger_char), "{t:?}");
        }
        for t in ["q", "_", ".", "\"", "9"] {
            assert!(t.chars().any(is_trigger_char), "{t:?}");
        }
    }

    fn marked(query: &str, label: &str) -> Vec<usize> {
        let item = CompletionItem {
            label: label.into(),
            ..Default::default()
        };
        highlight_subsequence(query, &item)
            .into_iter()
            .map(|r| r.start)
            .collect()
    }

    #[test]
    fn marks_the_typed_letters_in_order() {
        assert_eq!(marked("scr", "_score"), [1, 2, 4]);
        assert_eq!(marked("MUST", "must_not"), [0, 1, 2, 3]);
        assert!(marked("zz", "must").is_empty());
        assert!(marked("", "must").is_empty());
    }
}
