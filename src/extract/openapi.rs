use anyhow::Result;

use super::{Extractor, RawChunk};
use crate::source::Source;

pub struct OpenApiExtractor;

impl Extractor for OpenApiExtractor {
    fn can_handle(&self, source: &Source) -> bool {
        source.format_hint() == Some("openapi")
    }

    fn extract(&self, _source: &Source) -> Result<Vec<RawChunk>> {
        todo!("openapi extraction")
    }
}
