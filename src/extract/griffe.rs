use anyhow::Result;

use super::{Extractor, RawChunk};
use crate::source::Source;

pub struct GriffeExtractor;

impl Extractor for GriffeExtractor {
    fn can_handle(&self, source: &Source) -> bool {
        source.detected_language() == Some("python")
    }

    fn extract(&self, _source: &Source) -> Result<Vec<RawChunk>> {
        todo!("griffe extraction")
    }
}
