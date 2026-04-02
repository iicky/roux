use anyhow::Result;
use clap::Parser;

use roux_cli::cli::Cli;

fn main() -> Result<()> {
    let cli = Cli::parse();
    cli.run()
}
