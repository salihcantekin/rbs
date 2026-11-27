//! Regenerate the checked-in, type-derived method schemas (one file per namespace).
//! Usage: `cargo run -p rbs-api --example generate-schemas --features schema`.

use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas");
    rbs_api::schema::write_shards(&dir)
}
