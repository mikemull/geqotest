use crate::schema::Schema;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: Schema,
    pub seed: u64,
    pub skew: f64,
    pub filter_selectivity: f64,
    pub pg_schema: String,
    pub generated_at: String,
}

pub fn save(manifest: &Manifest, out_dir: &Path) -> anyhow::Result<()> {
    let path = out_dir.join("manifest.json");
    std::fs::write(path, serde_json::to_string_pretty(manifest)?)?;
    Ok(())
}

pub fn load(out_dir: &Path) -> anyhow::Result<Manifest> {
    let path = out_dir.join("manifest.json");
    let data = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("could not read {}: {e}", path.display()))?;
    Ok(serde_json::from_str(&data)?)
}
