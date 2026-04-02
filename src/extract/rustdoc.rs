use anyhow::Result;

use super::{Extractor, RawChunk};
use crate::source::{Source, SourceKind};

pub struct RustdocExtractor;

impl Extractor for RustdocExtractor {
    fn can_handle(&self, source: &Source) -> bool {
        matches!(source.kind, SourceKind::Crate(_))
            || source.format_hint() == Some("rustdoc")
            || source.detected_language() == Some("rust")
    }

    fn extract(&self, _source: &Source) -> Result<Vec<RawChunk>> {
        todo!("rustdoc extraction")
    }
}
