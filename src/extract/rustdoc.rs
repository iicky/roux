use anyhow::Result;

use super::{Extractor, RawChunk};
use crate::source::Source;

pub struct RustdocExtractor;

impl Extractor for RustdocExtractor {
    fn can_handle(&self, _source: &Source) -> bool {
        todo!()
    }

    fn extract(&self, _source: &Source) -> Result<Vec<RawChunk>> {
        todo!()
    }
}
