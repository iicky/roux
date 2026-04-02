use anyhow::Result;

use super::{Extractor, RawChunk};
use crate::source::Source;

pub struct TypeScriptExtractor;

impl Extractor for TypeScriptExtractor {
    fn can_handle(&self, source: &Source) -> bool {
        source.detected_language() == Some("typescript")
    }

    fn extract(&self, _source: &Source) -> Result<Vec<RawChunk>> {
        todo!("typescript extraction")
    }
}
