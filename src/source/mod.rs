use std::path::PathBuf;

pub struct Source {
    pub name: String,
    pub version: Option<String>,
    pub kind: SourceKind,
    pub language: Option<String>,
}

pub enum SourceKind {
    Crate(String),
    LocalPath(PathBuf),
    Url(String),
    File(PathBuf),
}
