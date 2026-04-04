use anyhow::Result;
use clap::Parser;

use roux_cli::cli::Cli;

fn main() -> Result<()> {
    // Use a large stack for the main thread to handle deeply nested ASTs
    let builder = std::thread::Builder::new().stack_size(64 * 1024 * 1024);
    let handler = builder
        .spawn(|| -> Result<()> {
            let cli = Cli::parse();
            cli.run()
        })
        .expect("failed to spawn main thread");

    handler.join().expect("main thread panicked")
}
