#![allow(dead_code)]

mod cli;
mod config;
mod embed;
mod extract;
mod model;
mod query;
mod source;
mod store;

use anyhow::Result;
use clap::Parser;

use cli::Cli;

fn main() -> Result<()> {
    let cli = Cli::parse();
    cli.run()
}
