mod assets;
mod completion;
mod connect;
mod console;
mod history_list;
mod hits_table;
mod library;
mod lsp_client;
mod paint;
mod targets;

use anyhow::{Context as _, Result};
use es_dsl_core::{Index, parse_version};
use gpui_kit::component::{GlobalState, TitleBar};
use gpui_kit::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};

fn main() -> Result<()> {
    // The index comes from `es-deck-schema`. Without an argument, look next to the working directory,
    // the executable, then the user config directory; the version always comes from the connected cluster.
    let config = dirs::config_dir()
        .context("no user config directory")?
        .join("elasticsearch-deck");
    let path = match std::env::args().nth(1) {
        Some(p) => std::path::PathBuf::from(p),
        None => [
            Some(std::path::PathBuf::from("index.json")),
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.join("index.json"))),
            Some(config.join("index.json")),
        ]
        .into_iter()
        .flatten()
        .find(|p| p.is_file())
        .context(
            "no index.json found; generate it with \
             `cargo run -p es-deck-schema -- <schema.json> index.json` \
             (schema.json is in the elasticsearch-specification repo, output/schema/) \
             and put it in the working directory or the app config directory",
        )?,
    };
    let index: Index = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("cannot read the index {path:?}"))?,
    )
    .with_context(|| format!("parsing {path:?}"))?;
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
            // Bindings first, then menus; `AppMenuBar` (Windows and Linux) reads them from the global state.
            cx.set_menus(console::app_menus());
            GlobalState::global_mut(cx).set_app_menus(
                console::app_menus()
                    .into_iter()
                    .map(|m| m.owned())
                    .collect(),
            );
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
