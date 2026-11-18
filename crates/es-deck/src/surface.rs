//! The island surfaces: the window ground, the panels that float on it, and the tones inside
//! them. One place decides how those values relate, so a new panel or row cannot drift from the
//! rest of the window.

use gpui_kit::Hsla;
use gpui_kit::component::Theme;

/// The surfaces the island layout paints with.
///
/// The hierarchy is the same in both modes: the ground is the window frame, panels sit above it,
/// and headers and insets sit above a panel. In dark mode the theme background is nearly black,
/// so a panel is the lighter tone; in light mode the panel is white and the ground is a shade
/// darker. The ground and panel relationship must not flip between modes, or a panel reads as a
/// hole rather than a floating surface.
#[derive(Debug, Clone, Copy)]
pub struct Surfaces {
    /// The window frame, also used by the title bar and status bar.
    pub ground: Hsla,
    /// The card the editor, tree and response live in.
    pub panel: Hsla,
    /// A header inside a panel, such as a table's column row.
    pub header: Hsla,
    /// A selected or grouped control inside a panel, such as the active console tab.
    pub inset: Hsla,
    /// A popover that floats above everything, such as completion or a menu.
    pub elevated: Hsla,
    /// A hover tint that can be layered over any surface.
    pub hover: Hsla,
    /// The selection tint used by rows and tabs.
    pub selected: Hsla,
}

impl Surfaces {
    /// Derives the surfaces from the active theme. The mode decides the direction; the theme
    /// decides the starting colours.
    pub fn new(theme: &Theme) -> Self {
        derive(
            theme.background,
            theme.foreground,
            theme.blue,
            theme.is_dark(),
        )
    }
}

fn derive(background: Hsla, foreground: Hsla, blue: Hsla, dark: bool) -> Surfaces {
    if dark {
        // Panels lift off the near-black ground; headers, insets and popovers lift again so they
        // stay distinguishable without borders or shadows.
        let panel = background.blend(foreground.opacity(0.07));
        Surfaces {
            ground: background,
            panel,
            header: panel.blend(foreground.opacity(0.04)),
            inset: panel.blend(foreground.opacity(0.06)),
            elevated: panel.blend(foreground.opacity(0.09)),
            hover: foreground.opacity(0.06),
            selected: blue.opacity(0.16),
        }
    } else {
        // Panels are white; the ground is a shade darker, so a panel's edge needs no shadow.
        // Insets are darker than the panel here, which is the correct direction for a recess.
        let ground = background.blend(foreground.opacity(0.06));
        Surfaces {
            ground,
            panel: background,
            header: background.blend(foreground.opacity(0.025)),
            inset: background.blend(foreground.opacity(0.04)),
            elevated: background,
            hover: foreground.opacity(0.05),
            selected: blue.opacity(0.12),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::rgb;

    fn color(hex: u32) -> Hsla {
        rgb(hex).into()
    }

    fn surfaces(dark: bool) -> Surfaces {
        let (background, foreground) = if dark {
            (color(0x0a0a0a), color(0xfafafa))
        } else {
            (color(0xffffff), color(0x0a0a0a))
        };
        derive(background, foreground, color(0x60a5fa), dark)
    }

    /// A panel must read as a surface above the ground in both modes; inverting it is what turns
    /// the cards into holes in dark mode.
    #[test]
    fn panels_sit_above_the_ground_in_both_modes() {
        for dark in [true, false] {
            let s = surfaces(dark);
            assert!(
                s.panel.l > s.ground.l + 0.02,
                "panel must be visibly lighter than the ground (dark: {dark})"
            );
        }
    }

    /// Elevation goes one way in dark mode and the other in light mode, where a recess is darker.
    #[test]
    fn inner_surfaces_follow_the_mode() {
        let dark = surfaces(true);
        assert!(dark.header.l > dark.panel.l);
        assert!(dark.inset.l > dark.header.l);
        assert!(dark.elevated.l > dark.inset.l);

        let light = surfaces(false);
        assert!(light.header.l < light.panel.l);
        assert!(light.inset.l < light.header.l);
        assert_eq!(light.elevated, light.panel);
    }

    #[test]
    fn the_ground_keeps_the_theme_background_in_dark_mode() {
        let dark = surfaces(true);
        assert_eq!(dark.ground, color(0x0a0a0a));
        assert_eq!(dark.panel.a, 1.0);
        assert_eq!(dark.ground.a, 1.0);
    }
}
