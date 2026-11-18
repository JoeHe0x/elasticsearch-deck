//! Settings, built from the kit's `Settings`: a page of groups and items on the left, a search
//! that filters them, and the values read from the console rather than a copy. A field writes
//! through the console, so a change here is the same change the menu bar makes.

use gpui_kit::assets::IconName;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::setting::{
    SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::{App, ParentElement as _, SharedString, Styled as _, WeakEntity, Window, div, rems};

use crate::appearance::Pref;
use crate::console::Console;

/// Opens Settings as a dialog. The console owns the values; the console is behind a weak handle
/// because the dialog can outlive it while it closes.
pub fn show(console: WeakEntity<Console>, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, window, _| {
        dialog
            .title("Settings")
            .w(rems(52.).to_pixels(window.rem_size()))
            .child(
                div()
                    .h(rems(30.))
                    .child(Settings::new("settings").page(appearance_page(console.clone()))),
            )
    });
}

fn appearance_page(console: WeakEntity<Console>) -> SettingPage {
    let read = console.clone();
    let write = console;
    SettingPage::new("Appearance")
        .icon(IconName::Palette)
        .description("How the console looks on screen.")
        .group(
            SettingGroup::new().title("Theme").item(
                SettingItem::new(
                    "Theme",
                    SettingField::dropdown(
                        [Pref::System, Pref::Light, Pref::Dark]
                            .into_iter()
                            .map(|pref| {
                                (
                                    SharedString::from(pref.key()),
                                    SharedString::from(pref.label()),
                                )
                            })
                            .collect(),
                        move |cx: &App| {
                            let key = read
                                .read_with(cx, |console, _| console.appearance().key())
                                .unwrap_or_else(|_| Pref::default().key());
                            SharedString::from(key)
                        },
                        move |value: SharedString, cx: &mut App| {
                            let Some(pref) = Pref::from_key(&value) else {
                                return;
                            };
                            let _ =
                                write.update(cx, |console, cx| console.set_theme(pref, None, cx));
                        },
                    )
                    .default_value(Pref::System.key()),
                )
                .description("Follow system changes with the operating system appearance."),
            ),
        )
}
