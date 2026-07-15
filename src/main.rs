use anyhow::Result;
use clap::Parser;

use roux_cli::cli::Cli;

fn main() -> Result<()> {
    // Use a large stack for the main thread to handle deeply nested ASTs
    // (same knob the extraction worker uses; see roux_cli::settings).
    let builder =
        std::thread::Builder::new().stack_size(roux_cli::settings::get().worker_stack_bytes);
    let handler = builder
        .spawn(|| -> Result<()> {
            let cli = Cli::parse();
            cli.run()
        })
        .expect("failed to spawn main thread");

    handler.join().expect("main thread panicked")
}
