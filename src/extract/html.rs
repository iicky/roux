use anyhow::Result;

use super::{Extractor, RawChunk};
use crate::source::Source;

pub struct HtmlExtractor;

impl Extractor for HtmlExtractor {
    fn can_handle(&self, source: &Source) -> bool {
        source.format_hint() == Some("html")
    }

    fn extract(&self, _source: &Source) -> Result<Vec<RawChunk>> {
        todo!("html extraction")
    }
}
