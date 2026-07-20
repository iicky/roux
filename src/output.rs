//! User-facing CLI status output, per `docs/cli-style.md`.
//!
//! Status, progress, warnings, and hints go to **stderr**. Program *data* —
//! query results, JSON, skeleton/compact blocks, the `list` table, export
//! paths — is written to **stdout** by the command handlers and never passes
//! through here, so `roux query … --format json` stays machine-clean.
//!
//! Styling is enabled only when stderr is a TTY and `NO_COLOR` is unset (wired
//! in [`init`]), so piped or redirected output degrades to plain text.

use std::fmt::Display;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use colored::Colorize;

/// roux's brand mark — the single source of truth for the glyph (mirrors
/// `logo.svg`). Change this one constant to rebrand the CLI.
pub const MARK: &str = "❖";

/// Roux amber (`#C87D3A`), the brand color for the completion mark.
const AMBER: (u8, u8, u8) = (0xC8, 0x7D, 0x3A);

/// CLI verbosity, set once from the global flags. `Quiet < Normal < Verbose`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Quiet = 0,
    Normal = 1,
    Verbose = 2,
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Normal as u8);

/// Set verbosity only (no color side effects) — used by the CLI and tests.
pub fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Configure verbosity and stderr styling. Call once at startup. Styling is on
/// only when stderr is a terminal and `NO_COLOR` is unset.
pub fn init(level: Level) {
    set_level(level);
    let colorize = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    colored::control::set_override(colorize);
}

fn level() -> Level {
    match LEVEL.load(Ordering::Relaxed) {
        0 => Level::Quiet,
        2 => Level::Verbose,
        _ => Level::Normal,
    }
}

/// True when verbosity `current` is at or above `min`. Pure, so the gating
/// logic is testable without mutating the global level.
fn visible(current: Level, min: Level) -> bool {
    current >= min
}

/// True when the current verbosity is at or above `min`.
fn at_least(min: Level) -> bool {
    visible(level(), min)
}

/// A completed action, branded with the roux mark. Hidden under `--quiet`.
pub fn done(msg: impl Display) {
    if at_least(Level::Normal) {
        let (r, g, b) = AMBER;
        eprintln!("{} {msg}", MARK.truecolor(r, g, b));
    }
}

/// A sub-step that succeeded. Hidden under `--quiet`.
pub fn ok(msg: impl Display) {
    if at_least(Level::Normal) {
        eprintln!("{} {msg}", "ok".green().bold());
    }
}

/// Routine progress (no prefix). Hidden under `--quiet`.
pub fn step(msg: impl Display) {
    if at_least(Level::Normal) {
        eprintln!("{msg}");
    }
}

/// A non-fatal issue the user should see. Shown even under `--quiet`.
pub fn warn(msg: impl Display) {
    eprintln!("{} {msg}", "warn".yellow().bold());
}

/// A failure line. The handler still returns `Err` for the exit code; this only
/// prints the message. Shown even under `--quiet`.
pub fn error(msg: impl Display) {
    eprintln!("{} {msg}", "error".red().bold());
}

/// A next-action suggestion, pairing with a `warn`/`error`. Shown even under `--quiet`.
pub fn hint(msg: impl Display) {
    eprintln!("{} {msg}", "hint".cyan().bold());
}

/// Verbose-only detail. Shown only under `--verbose`.
pub fn detail(msg: impl Display) {
    if at_least(Level::Verbose) {
        eprintln!("{}", msg.to_string().dimmed());
    }
}

/// A live progress spinner for a long, otherwise-silent operation. On a terminal
/// it animates until dropped; when stderr is not a terminal it degrades to a
/// single [`step`] line so pipes and logs still record the operation. Silent
/// under `--quiet`. Clears itself on drop so the next status line prints clean.
pub struct Spinner {
    stop: Option<Arc<AtomicBool>>,
    handle: Option<JoinHandle<()>>,
}

/// Start a [`Spinner`] labeled `msg`. Keep the returned guard alive for the
/// duration of the work; dropping it stops and clears the animation. Uses only
/// `\r` and spaces (no ANSI), so it renders on every terminal, Windows included.
pub fn spinner(msg: impl Display) -> Spinner {
    if !at_least(Level::Normal) {
        return Spinner {
            stop: None,
            handle: None,
        };
    }
    if !std::io::stderr().is_terminal() {
        // Not a terminal: a spinning animation is noise in a pipe or log; emit
        // one plain line so the operation is still recorded.
        step(msg);
        return Spinner {
            stop: None,
            handle: None,
        };
    }
    let msg = msg.to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let handle = std::thread::spawn(move || {
        const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
        let mut err = std::io::stderr();
        let mut i = 0usize;
        while !stop_thread.load(Ordering::Relaxed) {
            let _ = write!(err, "\r{} {msg}", FRAMES[i % FRAMES.len()]);
            let _ = err.flush();
            i += 1;
            std::thread::sleep(Duration::from_millis(90));
        }
        // Overwrite the line with spaces and return the cursor so the next
        // status line prints clean, without relying on ANSI clear sequences.
        let _ = write!(err, "\r{}\r", " ".repeat(msg.chars().count() + 2));
        let _ = err.flush();
    });
    Spinner {
        stop: Some(stop),
        handle: Some(handle),
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_hides_routine_and_detail() {
        // done/ok/step gate on Normal; detail gates on Verbose.
        assert!(
            !visible(Level::Quiet, Level::Normal),
            "quiet hides done/ok/step"
        );
        assert!(!visible(Level::Quiet, Level::Verbose), "quiet hides detail");
    }

    #[test]
    fn normal_shows_routine_hides_detail() {
        assert!(
            visible(Level::Normal, Level::Normal),
            "normal shows done/ok/step"
        );
        assert!(
            !visible(Level::Normal, Level::Verbose),
            "normal hides detail"
        );
    }

    #[test]
    fn verbose_shows_everything() {
        assert!(visible(Level::Verbose, Level::Normal));
        assert!(
            visible(Level::Verbose, Level::Verbose),
            "verbose shows detail"
        );
    }
}
