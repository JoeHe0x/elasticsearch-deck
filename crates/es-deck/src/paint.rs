//! Syntax colors and per-request bars for the console editor. The editor has no console grammar,
//! so the spans come from `es-dsl-core` and are laid over the text as decorations.

use std::cell::RefCell;
use std::ops::Range;

use es_dsl_core::{Token, json_tokens, tokens};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::{
    EditorState, RangeDecoration, RangeDecorationCollection, RangeDecorationStyle, TextDecoration,
    TextDecorationCollection,
};
use gpui_kit::{App, Entity, FontWeight, HighlightStyle, Hsla, rgb};

pub struct Paint {
    colors: TextDecorationCollection,
    /// A tint behind the request the cursor is in.
    active: RangeDecorationCollection,
    /// A response body: plain JSON, with no request lines.
    json_only: bool,
    /// Keys the user chose, from the last schema-aware pass; they lag edits until the next one.
    custom: RefCell<Vec<(Range<usize>, Token)>>,
}

/// Kibana Console's roles (method accent, url accent-secondary, keys danger, values primary, numbers
/// accent) with the Borealis hex values of EUI's light (`*100`) and dark (`*60`) text colors.
/// Fixed rather than taken from the theme's accents, which are tuned for UI chrome.
pub fn color(token: Token, dark: bool) -> Hsla {
    let (d, l) = match token {
        Token::Method | Token::Number => (0xEE72A6, 0xA11262),
        Token::Target => (0xFCD883, 0x825803),
        Token::Endpoint => (0x16C5C0, 0x047471),
        Token::Query => (0x98A8C3, 0x516381),
        Token::Key => (0xF6726A, 0xA71627),
        Token::Property => (0xB794F6, 0x6F42C1),
        Token::Field => (0x24C292, 0x09724D),
        Token::String | Token::Keyword => (0x61A2FF, 0x1750BA),
    };
    rgb(if dark { d } else { l }).into()
}

/// The editor's style for `token`. Shared with surfaces that show a request line outside the
/// editor — the title bar's crumb — so both read the same instead of one flat string.
pub fn style(token: Token, dark: bool) -> HighlightStyle {
    HighlightStyle {
        color: Some(color(token, dark)),
        font_weight: match token {
            Token::Method => Some(FontWeight::BOLD),
            Token::Endpoint | Token::Property => Some(FontWeight::MEDIUM),
            _ => None,
        },
        ..Default::default()
    }
}

impl Paint {
    pub fn new(editor: &Entity<EditorState>, json_only: bool, cx: &mut App) -> Self {
        editor.update(cx, |state, cx| Self {
            json_only,
            custom: RefCell::default(),
            colors: state.create_decorations_collection(Vec::new(), cx),
            active: state.create_range_decorations_collection(Vec::new(), cx),
        })
    }

    /// Records the schema's classes of keys and repaints.
    pub fn set_custom(&self, text: &str, custom: Vec<(Range<usize>, Token)>, cx: &mut App) {
        *self.custom.borrow_mut() = custom;
        self.refresh(text, cx);
    }

    /// Recomputes every span; call after each edit (the ranges would follow edits, but not re-classify).
    pub fn refresh(&self, text: &str, cx: &mut App) {
        let theme = cx.theme();
        let dark = theme.is_dark();
        let custom = self.custom.borrow();
        let spans = if self.json_only {
            json_tokens(text)
        } else {
            tokens(text)
        };
        let colors = spans
            .into_iter()
            .map(|(range, token)| {
                let token = match custom.binary_search_by_key(&range.start, |(r, _)| r.start) {
                    Ok(i) if token == Token::Key && custom[i].0.end == range.end => custom[i].1,
                    _ => token,
                };
                TextDecoration::new(range, style(token, dark))
            })
            .collect();
        self.colors.set(colors, cx);
    }

    /// Tints `block`, or clears the tint. Callers skip unchanged ranges: setting notifies the editor.
    /// A frame would follow each line's own width and step along the request's ragged right edge,
    /// so the request is filled instead; the purple is light enough to sit behind the text colors.
    pub fn set_active(&self, block: Option<Range<usize>>, cx: &mut App) {
        let alpha = if cx.theme().is_dark() { 0.18 } else { 0.10 };
        let color = Hsla::from(rgb(0x8B5CF6)).opacity(alpha);
        let tint = block.map(|r| {
            RangeDecoration::new(r)
                .with_style(RangeDecorationStyle::Fill)
                .with_color(color)
        });
        self.active.set(tint.into_iter().collect(), cx);
    }
}
