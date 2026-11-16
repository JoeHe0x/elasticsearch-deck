use anyhow::{Context, Result};
use es_dsl_core::{Index, parse_version};
use lsp_server::Connection;

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: es-dsl-lsp <index.json> [stack-version]")?;
    let version = match std::env::args().nth(2) {
        Some(v) => Some(parse_version(&v).with_context(|| format!("bad version {v:?}"))?),
        None => None,
    };
    let index: Index = serde_json::from_slice(&std::fs::read(&path).with_context(|| path.clone())?)
        .with_context(|| format!("parsing {path}"))?;
    let (conn, io) = Connection::stdio();
    es_dsl_lsp::serve(&conn, &index, version)?;
    drop(conn);
    io.join()?;
    Ok(())
}
