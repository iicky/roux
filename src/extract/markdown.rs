use anyhow::Result;

use super::{Extractor, RawChunk};
use crate::source::Source;

pub struct MarkdownExtractor;

impl Extractor for MarkdownExtractor {
    fn can_handle(&self, source: &Source) -> bool {
        source.format_hint() == Some("markdown")
    }

    fn extract(&self, _source: &Source) -> Result<Vec<RawChunk>> {
        todo!("markdown extraction")
    }
}
