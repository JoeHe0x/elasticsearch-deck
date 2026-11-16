use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output] = args.as_slice() else {
        bail!("usage: es-deck-schema <schema.json> <index.json>");
    };
    let spec = std::fs::read_to_string(input).with_context(|| format!("reading {input}"))?;
    let index = es_deck_schema::convert(&spec)?;
    std::fs::write(output, serde_json::to_vec(&index)?)
        .with_context(|| format!("writing {output}"))?;
    eprintln!(
        "{} endpoints, {} types",
        index.endpoints.len(),
        index.types.len()
    );
    Ok(())
}
