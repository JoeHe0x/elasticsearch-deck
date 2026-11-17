//! Syntax colors and per-request bars for the console editor. The editor has no console grammar,
//! so the spans come from `es-dsl-core` and are laid over the text as decorations.

use std::ops::Range;

use es_dsl_core::{Token, block_spans, tokens};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::{
    EditorState, GutterDecoration, GutterDecorationCollection, RangeDecoration,
    RangeDecorationCollection, TextDecoration, TextDecorationCollection,
};
use gpui_kit::{App, Entity, FontWeight, HighlightStyle, Hsla};

pub struct Paint {
    colors: TextDecorationCollection,
    bars: GutterDecorationCollection,
    /// A frame around the request the cursor is in.
    active: RangeDecorationCollection,
}

impl Paint {
    pub fn new(editor: &Entity<EditorState>, cx: &mut App) -> Self {
        editor.update(cx, |state, cx| Self {
            colors: state.create_decorations_collection(Vec::new(), cx),
            bars: state.create_gutter_decorations_collection(Vec::new(), cx),
            active: state.create_range_decorations_collection(Vec::new(), cx),
        })
    }

    /// Recomputes every span; call after each edit (the ranges would follow edits, but not re-classify).
    pub fn refresh(&self, text: &str, cx: &mut App) {
        let theme = cx.theme();
        let (accent, muted) = (theme.blue, theme.muted_foreground);
        let style = |token| {
            let color = match token {
                Token::Method => theme.magenta,
                Token::Path => theme.cyan,
                Token::Query => muted,
                Token::Key => theme.blue,
                Token::String => theme.green,
                Token::Number => theme.yellow,
                Token::Keyword => theme.red,
            };
            HighlightStyle {
                color: Some(color),
                font_weight: (token == Token::Method).then_some(FontWeight::BOLD),
                ..Default::default()
            }
        };
        let colors = tokens(text)
            .into_iter()
            .map(|(range, token)| TextDecoration::new(range, style(token)))
            .collect();
        let bars = block_spans(text)
            .into_iter()
            .map(|range| GutterDecoration::new(range).with_color(accent))
            .collect();
        self.colors.set(colors, cx);
        self.bars.set(bars, cx);
    }

    /// Frames `block`, or clears the frame. Callers skip unchanged ranges: setting notifies the editor.
    /// Pale lavender: the default frame color is the editor foreground, which is near black.
    pub fn set_active(&self, block: Option<Range<usize>>, cx: &mut App) {
        let color = Hsla {
            h: 0.70,
            s: 0.75,
            l: 0.78,
            a: 1.0,
        };
        let frame = block.map(|r| RangeDecoration::new(r).with_color(color));
        self.active.set(frame.into_iter().collect(), cx);
    }
}
