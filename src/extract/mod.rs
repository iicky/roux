pub mod godoc;
pub mod griffe;
pub mod html;
pub mod markdown;
pub mod openapi;
pub mod rustdoc;
pub mod treesitter;
pub mod typescript;

use anyhow::Result;

use crate::source::Source;

pub struct RawChunk {
    pub source_name: String,
    pub source_version: String,
    pub language: String,
    pub item_type: String,
    pub qualified_name: String,
    pub signature: Option<String>,
    pub doc: String,
    pub body: String,
    pub url: Option<String>,
}

pub trait Extractor {
    fn can_handle(&self, source: &Source) -> bool;
    fn extract(&self, source: &Source) -> Result<Vec<RawChunk>>;
}

pub fn registry() -> Vec<Box<dyn Extractor>> {
    vec![
        Box::new(rustdoc::RustdocExtractor),
        Box::new(markdown::MarkdownExtractor),
        Box::new(html::HtmlExtractor),
        Box::new(openapi::OpenApiExtractor),
        Box::new(griffe::GriffeExtractor),
        Box::new(typescript::TypeScriptExtractor),
        Box::new(godoc::GoDocExtractor),
        Box::new(treesitter::TreeSitterExtractor), // fallback, last
    ]
}

pub fn extract(source: &Source) -> Result<Vec<RawChunk>> {
    let extractors = registry();
    for ext in &extractors {
        if ext.can_handle(source) {
            return ext.extract(source);
        }
    }
    anyhow::bail!("no extractor found for source: {}", source.name)
}
