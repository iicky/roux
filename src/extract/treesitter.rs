use anyhow::Result;

use super::{Extractor, RawChunk};
use crate::source::Source;

pub struct TreeSitterExtractor;

impl Extractor for TreeSitterExtractor {
    fn can_handle(&self, _source: &Source) -> bool {
        true // fallback — always accepts
    }

    fn extract(&self, _source: &Source) -> Result<Vec<RawChunk>> {
        todo!("treesitter extraction")
    }
}
