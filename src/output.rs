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
use std::io::IsTerminal;
use std::sync::atomic::{AtomicU8, Ordering};

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
