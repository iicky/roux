use anyhow::Result;

use super::{Extractor, RawChunk};
use crate::source::Source;

pub struct GoDocExtractor;

impl Extractor for GoDocExtractor {
    fn can_handle(&self, source: &Source) -> bool {
        source.detected_language() == Some("go")
    }

    fn extract(&self, _source: &Source) -> Result<Vec<RawChunk>> {
        todo!("godoc extraction")
    }
}
