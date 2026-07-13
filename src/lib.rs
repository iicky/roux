//! # roux
//!
//! Graph-native code retrieval for AI coding agents.
//!
//! `roux` extracts a symbol graph from source with tree-sitter, indexes it with
//! full-text (BM25) search in SQLite, and ranks queries with Personalized
//! PageRank over the graph — returning matched symbols together with their
//! call/definition neighborhood. It is CPU-only and needs no embeddings, GPU,
//! or model download.
//!
//! This crate is primarily the implementation behind the `roux` command-line
//! binary; see the [README](https://github.com/iicky/roux#readme) for
//! user-facing docs. The modules below are the internals, exposed for testing
//! and embedding:
//!
//! - [`graph`] — symbol/edge extraction ([`graph::extract`]), the SQLite-backed
//!   [`GraphStore`](graph::store::GraphStore), and BM25 + PPR ranking
//!   ([`graph::rank`]).
//! - [`source`] — ingestion of crates and local paths into the store.
//! - [`config`] and [`settings`] — store-scope resolution and the tunable
//!   retrieval parameters (all overridable via `ROUX_*` environment variables).
//! - [`cli`] — argument parsing and command handlers.
//! - [`mcp`] — the Model Context Protocol server behind `roux serve`.
//! - [`artifact`], [`lockfile`], and [`fingerprint`] — index export, dependency
//!   tracking, and change detection.

#![allow(dead_code, clippy::too_many_arguments, clippy::only_used_in_recursion)]

pub mod artifact;
pub mod cli;
pub mod config;
pub mod fingerprint;
pub mod graph;
pub mod lockfile;
pub mod mcp;
pub mod settings;
pub mod source;
