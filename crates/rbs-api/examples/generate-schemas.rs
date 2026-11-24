//! Regenerate the checked-in, type-derived method schemas.

use std::io::{self, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schemas = rbs_api::schema::generate_schemas()?;
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, &schemas)?;
    writeln!(out)?;
    Ok(())
}
