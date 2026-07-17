use std::process::ExitCode;

use clap::Parser;

use roux_cli::cli::Cli;

fn main() -> ExitCode {
    // Use a large stack for the main thread to handle deeply nested ASTs
    // (same knob the extraction worker uses; see roux_cli::settings).
    let builder =
        std::thread::Builder::new().stack_size(roux_cli::settings::get().worker_stack_bytes);
    let handler = builder
        .spawn(|| Cli::parse().run())
        .expect("failed to spawn main thread");

    match handler.join().expect("main thread panicked") {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Print the whole anyhow context chain on one line, branded via the
            // `error` prefix, then exit non-zero. Printing here rather than
            // returning `Err` avoids the default `Error: ...` Debug dump landing
            // on top of our branded line. Clap parse errors exit earlier, inside
            // `Cli::parse`, and keep clap's own formatting.
            roux_cli::output::error(format!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}
