//! Tiny in-memory graph library used as an extractor fixture.
//!
//! Exercises module structure, re-exports, doc comments, generics,
//! traits, and impls.

pub mod parser;
pub mod types;

pub use parser::ParserBuilder;
pub use types::{Graph, GraphError, Node};
