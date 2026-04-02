use std::path::PathBuf;

use anyhow::Result;

pub fn model_dir(model_id: &str) -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("roux")
        .join("models")
        .join(model_id.replace('/', "-"))
}

pub fn download(_model_id: &str) -> Result<PathBuf> {
    todo!()
}

pub fn status(_model_id: &str) -> Result<String> {
    todo!()
}
