// Release builds on Windows are GUI programs; without this, Windows opens a console window behind the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod appearance;
mod assets;
mod completion;
mod connect;
mod console;
mod data_browser;
mod history_list;
mod hits_table;
mod library;
mod lsp_client;
mod paint;
mod schema_source;
mod surface;
mod targets;

use anyhow::{Context as _, Result};
use es_dsl_core::{Index, parse_version};
use gpui_kit::component::TitleBar;
use gpui_kit::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};

fn main() -> Result<()> {
    let config = dirs::config_dir()
        .context("no user config directory")?
        .join("elasticsearch-deck");
    // The index for the connected version is downloaded on connect; this one is used until then and
    // when that fails. An argument replaces it, for debugging.
    let index: Index = match std::env::args().nth(1) {
        Some(path) => serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("cannot read the index {path:?}"))?,
        )
        .with_context(|| format!("parsing {path:?}"))?,
        None => serde_json::from_slice(include_bytes!("../assets/index-8.19.json"))
            .context("parsing the bundled index")?,
    };
    // Optional override for debugging; connecting to a cluster replaces it with the cluster's version.
    let version = match std::env::args().nth(2) {
        Some(v) => Some(parse_version(&v).with_context(|| format!("bad version {v:?}"))?),
        None => None,
    };
    let client = lsp_client::LspClient::start(index.clone(), version, es_dsl_core::NoFields)?;
    let connections = config.join("connections.json");

    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            console::bind_keys(cx);
            // Before the menus: they show which appearance mode is current.
            let pref = appearance::Pref::load(&config.join("appearance.json"));
            // Bindings first, then menus; `AppMenuBar` (Windows and Linux) reads them from the global state.
            console::set_menus(cx, pref, false);
            // The default window fills the display or more; ask for a fixed size, centered, that
            // the platform clamps to the screen.
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(800.)),
                    cx,
                ))),
                ..TitleBar::window_options()
            };
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                let library = cx.new(|_| library::Library::load(&connections));
                let draft = library.read(cx).draft();
                cx.new(|cx| {
                    console::Console::new(&draft, index, client, library, connections, window, cx)
                })
            });
            if let Err(e) = opened {
                eprintln!("failed to open window: {e}");
                cx.quit();
            }
        });
    Ok(())
}
