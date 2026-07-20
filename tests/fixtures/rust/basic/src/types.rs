//! Core graph data types.

use std::collections::HashMap;

/// A node carrying an identifier and a label.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: u64,
    pub label: String,
}

/// A directed graph parameterized by node payload type.
pub struct Graph<T> {
    pub nodes: HashMap<u64, T>,
    pub edges: Vec<(u64, u64)>,
}

impl<T> Graph<T> {
    /// Build an empty graph.
    pub fn new() -> Self {
        Graph {
            nodes: HashMap::new(),
            edges: Vec::new(),
        }
    }

    /// Insert a node, replacing any existing entry with the same id.
    pub fn add_node(&mut self, id: u64, value: T) {
        self.nodes.insert(id, value);
    }

    /// Append a directed edge between two existing nodes.
    pub fn add_edge(&mut self, from: u64, to: u64) -> Result<(), GraphError> {
        if !self.nodes.contains_key(&from) {
            return Err(GraphError::NotFound(from));
        }
        if !self.nodes.contains_key(&to) {
            return Err(GraphError::NotFound(to));
        }
        self.edges.push((from, to));
        Ok(())
    }
}

impl<T> Default for Graph<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors raised during graph mutation.
#[derive(Debug)]
pub enum GraphError {
    NotFound(u64),
    DuplicateEdge,
}
