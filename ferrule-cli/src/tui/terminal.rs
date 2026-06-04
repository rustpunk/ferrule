//! Terminal setup / teardown for the TUI.
//!
//! The single most important correctness property of any TUI is that
//! the terminal is restored — raw mode off, alternate screen left — no
//! matter how the program exits. This module covers both exit paths:
//!
//! - **Normal return / `?` early return:** [`TerminalGuard`] restores in
//!   its `Drop` impl, which runs whenever the guard goes out of scope.
//! - **Panic / unwind:** `Drop` also runs during unwinding, *and*
//!   [`install_panic_hook`] restores the terminal before the default
//!   hook prints the panic message, so the backtrace is readable and the
//!   terminal is usable even if a panic happens mid-render.
//!
//! The actual restore sequence lives in [`restore`], a free function the
//! guard's `Drop` and the panic hook both call. It is idempotent —
//! calling it twice is harmless — which the test suite relies on (and
//! which matters because both the panic hook and `Drop` can fire for the
//! same panic).

use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout};

/// Restore the terminal to its pre-TUI state: leave the alternate
/// screen and disable raw mode.
///
/// Idempotent: safe to call when the terminal is already restored (a
/// second `disable_raw_mode` / `LeaveAlternateScreen` is a no-op or a
/// harmless error that is intentionally ignored). Both [`TerminalGuard`]
/// `Drop` and the panic hook installed by [`install_panic_hook`] call
/// this, so it must tolerate being run more than once.
pub fn restore() {
    // Errors here are swallowed deliberately: we are on a teardown /
    // unwind path and there is nothing useful to do with an error
    // except avoid masking the original cause of teardown.
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

/// Install a panic hook that restores the terminal before the previous
/// hook runs.
///
/// Without this, a panic during a ratatui `draw` would unwind with the
/// terminal still in raw / alternate-screen mode, and the panic message
/// would be printed into a corrupted display. Chaining preserves the
/// existing hook (miette's, or the default) so the panic is still
/// reported normally — just onto a restored terminal.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

/// RAII guard owning the ratatui [`Terminal`]. Constructing it enters
/// raw mode and the alternate screen; dropping it restores both.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Enter raw mode + the alternate screen and build the terminal.
    ///
    /// On any setup error this best-effort restores so a partial setup
    /// (e.g. raw mode enabled but the alternate-screen switch failed)
    /// does not leave the terminal wedged.
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            restore();
            return Err(e);
        }
        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(e) => {
                restore();
                Err(e)
            }
        }
    }

    /// Mutable access to the underlying terminal for `draw` calls.
    pub fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_is_idempotent() {
        // Calling restore() with no terminal attached (the test
        // harness's stdout is not a TTY) must not panic, and calling it
        // twice must be equally harmless. This is the property both the
        // Drop guard and the panic hook depend on.
        restore();
        restore();
    }

    #[test]
    fn install_panic_hook_chains_and_can_be_unwound() {
        // `install_panic_hook` replaces the global hook with one that
        // restores the terminal and then calls the previous hook. This
        // test verifies it installs without panicking and that the prior
        // hook can be put back, so it does not leak a restore-terminal
        // hook into the rest of the test process. (The hook itself only
        // runs on a real panic, which this test does not trigger.)
        // Snapshot is impossible (hooks are not Clone), so we simply
        // install ours, then take it back off. Taking a hook resets the
        // global slot to the default hook, which is a safe state for any
        // test that runs afterward.
        install_panic_hook();
        let _installed = std::panic::take_hook();
    }
}
