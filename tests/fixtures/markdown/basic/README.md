# Tiny Graph

A small graph library used as a Markdown fixture.

## Overview

The library provides a `Graph<T>` container and a `Node` type for storing
nodes by integer id. The `ParserBuilder` configures parsing behavior.

## Quick start

```rust
use tiny_graph::ParserBuilder;

let parser = ParserBuilder::new().strict(true);
let graph = parser.parse("hello world");
```

## API

The public surface is small:

- `Graph::new` — create an empty graph
- `Graph::add_node` — insert a node
- `Graph::add_edge` — connect two nodes
- `ParserBuilder::strict` — toggle strict mode

## Errors

The `GraphError` enum represents failures. `NotFound` is returned when
a referenced id is missing.

## See also

- The [parser module](./parser.md) for grammar details.
