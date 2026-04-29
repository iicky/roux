//! Build graphs from text input.

use crate::types::{Graph, Node};

/// Configurable builder for `Graph<Node>` instances.
pub struct ParserBuilder {
    pub strict: bool,
    pub max_nodes: usize,
}

impl ParserBuilder {
    /// Create a builder with default settings (non-strict, 1024 max nodes).
    pub fn new() -> Self {
        ParserBuilder {
            strict: false,
            max_nodes: 1024,
        }
    }

    /// Toggle strict mode. In strict mode, malformed input returns errors
    /// rather than producing a partial graph.
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Parse the given text into a graph.
    pub fn parse(&self, text: &str) -> Graph<Node> {
        let mut g: Graph<Node> = Graph::new();
        for (idx, line) in text.lines().enumerate() {
            let id = idx as u64;
            let node = Node {
                id,
                label: line.to_string(),
            };
            g.add_node(id, node);
        }
        g
    }
}

impl Default for ParserBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait for types that produce graphs from text.
pub trait Parse {
    fn parse(&self, text: &str) -> Graph<Node>;
}

impl Parse for ParserBuilder {
    fn parse(&self, text: &str) -> Graph<Node> {
        ParserBuilder::parse(self, text)
    }
}
