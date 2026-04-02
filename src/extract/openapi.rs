use anyhow::Result;

use super::{Extractor, RawChunk};
use crate::source::Source;

pub struct OpenApiExtractor;

impl Extractor for OpenApiExtractor {
    fn can_handle(&self, _source: &Source) -> bool {
        todo!()
    }

    fn extract(&self, _source: &Source) -> Result<Vec<RawChunk>> {
        todo!()
    }
}
