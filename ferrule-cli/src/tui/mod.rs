//! `ferrule tui <conn>` — a Harlequin-style terminal UI (issue #27).
//!
//! This is the first reviewable increment: a connected, navigable TUI
//! with a schema-tree pane, a query editor, and a results pane that
//! renders through the existing [`ferrule_core::formatter`]. It is
//! gated behind the off-by-default `tui` Cargo feature; the whole module
//! tree compiles only when that feature is on.
//!
//! # Architecture
//!
//! All logic lives in pure, terminal-free submodules so it is
//! unit-testable without a TTY or a database:
//!
//! - [`app`] — the [`App`] state struct and its pure transitions.
//! - [`schema_tree`] — the navigable schema/table model.
//! - [`input`] — the query editor buffer.
//! - [`event`] — `KeyEvent` -> [`event::KeyAction`] mapping.
//! - [`results`] — the formatted, scrollable result model.
//!
//! Terminal I/O is confined to the edges:
//!
//! - [`terminal`] — the RAII [`terminal::TerminalGuard`] plus the
//!   panic hook that restore the terminal on every exit path.
//! - [`ui`] — the pure render function.
//!
//! # Known limitations (deferred to follow-ups)
//!
//! - **Queries run synchronously on the event-loop thread.** A
//!   long-running query freezes the UI until it returns. Non-blocking
//!   execution is a deliberate follow-up, not a bug.
//! - Single query buffer (no tabs / multi-buffer), no SQL syntax
//!   highlighting, no autocompletion, keyboard-only (no mouse), and
//!   results scroll line-wise only (no server-side paging). These are
//!   named in the issue and intentionally out of scope for this
//!   increment.

pub mod app;
pub mod event;
pub mod input;
pub mod results;
pub mod schema_tree;
pub mod terminal;
pub mod ui;

use crate::commands::TuiArgs;
use crate::error::CliError;
use app::{App, Focus};
use crossterm::event::{self as cevent, Event, KeyCode};
use event::KeyAction;
use ferrule_config::profile::GlobalConfig;
use ferrule_sql::connection::ConnectOptions;
use is_terminal::IsTerminal;
use schema_tree::{ConnectionSchemaSource, SchemaTree};
use std::time::Duration;
use terminal::TerminalGuard;

/// How long [`cevent::read`] waits for input before the loop redraws.
/// A short poll keeps the UI responsive to terminal resizes without a
/// busy spin.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Entry point for `ferrule tui <conn>`.
///
/// Refuses to start without an interactive terminal, rejects `--daemon`
/// (a long-lived interactive session wants its own dedicated
/// connection, not a pooled one), resolves and opens the connection,
/// builds the schema tree, then runs the event loop under a
/// [`TerminalGuard`] so the terminal is always restored on exit.
pub fn run(args: TuiArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(CliError::usage(
            "The TUI requires an interactive terminal (both stdin and stdout \
             must be a TTY).",
        ));
    }

    if args.conn_flags.daemon {
        return Err(CliError::usage(
            "ferrule tui does not support --daemon. An interactive session \
             holds a dedicated connection for its lifetime; pooling it through \
             the daemon would tie the session to per-request connection \
             affinity. Drop --daemon.",
        ));
    }

    let resolved = crate::commands::resolve_connection(
        &args.connection,
        None,
        args.conn_flags.ssh_tunnel.as_deref(),
        args.conn_flags.ssh_key.as_deref(),
        args.conn_flags.proxy_url.as_deref(),
        global_config,
    )?;
    // Belt-and-suspenders: --daemon is already rejected above, but keep
    // the shared compatibility check so any future relaxation still
    // honours the SSH/daemon exclusion.
    crate::commands::check_daemon_ssh_compat(args.conn_flags.daemon, &resolved)?;

    if args.conn_flags.insecure {
        eprintln!("Warning: --insecure disables TLS certificate verification.");
    }

    let conn_label = resolved.url.redacted();

    let opts = ConnectOptions {
        insecure: args.conn_flags.insecure,
        password: None,
    };
    let mut conn = crate::commands::connect_resolved(resolved, &opts)?;

    // Build the schema tree from the live connection before taking over
    // the terminal, so a failure here surfaces as a normal CLI error
    // rather than from inside the alternate screen.
    let tree = {
        let mut source = ConnectionSchemaSource::new(conn.as_mut());
        SchemaTree::build(&mut source).map_err(CliError::query)?
    };

    let mut app = App::new(conn, conn_label, tree);

    // Restore the terminal on panic as well as on normal exit. The
    // guard's Drop covers normal/early returns and unwinding; the panic
    // hook ensures the terminal is restored *before* the panic message
    // prints.
    terminal::install_panic_hook();
    let mut guard = TerminalGuard::enter().map_err(CliError::Io)?;

    let loop_result = event_loop(&mut guard, &mut app);

    // `guard` drops here, restoring the terminal, before we propagate any
    // error so the diagnostic prints onto a sane terminal.
    drop(guard);
    loop_result
}

/// The draw / read / dispatch loop. Returns when the user quits.
fn event_loop(guard: &mut TerminalGuard, app: &mut App) -> Result<(), CliError> {
    while app.running() {
        guard
            .terminal_mut()
            .draw(|frame| ui::render(frame, app))
            .map_err(CliError::Io)?;

        // Poll so the loop wakes periodically to redraw (e.g. after a
        // resize) even when no key is pressed.
        if !cevent::poll(POLL_INTERVAL).map_err(CliError::Io)? {
            continue;
        }

        // Resize is handled implicitly by the next draw; other events
        // (mouse, paste, focus) are ignored in this increment, so we only
        // act on key presses.
        if let Event::Key(key) = cevent::read().map_err(CliError::Io)? {
            // Esc on an empty, input-focused buffer is a convenient quit;
            // everywhere else Esc is unmapped. This buffer check lives
            // here (not in `map_key`) so the mapping stays buffer-agnostic
            // and easy to test.
            if key.code == KeyCode::Esc && app.focus() == Focus::Input && app.input().is_empty() {
                app.quit();
                continue;
            }
            let action = event::map_key(key, app.focus());
            apply(app, action);
        }
    }
    Ok(())
}

/// Apply a decoded [`KeyAction`] to the application state.
fn apply(app: &mut App, action: KeyAction) {
    match action {
        KeyAction::Quit => app.quit(),
        KeyAction::FocusNext => app.focus_next(),
        KeyAction::RunQuery => app.run_query(),
        KeyAction::ScrollUp => app.results_mut().scroll_up(),
        KeyAction::ScrollDown => app.results_mut().scroll_down(),
        KeyAction::PageUp => app.results_mut().page_up(10),
        KeyAction::PageDown => app.results_mut().page_down(10),
        KeyAction::TreeUp => app.tree_mut().select_up(),
        KeyAction::TreeDown => app.tree_mut().select_down(),
        // Activating a schema row expands/collapses it; activating a
        // table row loads a starter SELECT into the editor.
        KeyAction::ToggleExpand => app.activate_tree_selection(),
        KeyAction::Char(c) => {
            app.clear_error_on_edit();
            app.input_mut().insert_char(c);
        }
        KeyAction::Backspace => {
            app.clear_error_on_edit();
            app.input_mut().backspace();
        }
        KeyAction::Delete => {
            app.clear_error_on_edit();
            app.input_mut().delete();
        }
        KeyAction::CursorLeft => app.input_mut().move_left(),
        KeyAction::CursorRight => app.input_mut().move_right(),
        KeyAction::CursorHome => app.input_mut().move_home(),
        KeyAction::CursorEnd => app.input_mut().move_end(),
        KeyAction::None => {}
    }
}
