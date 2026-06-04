//! Pure key-event mapping: crossterm [`KeyEvent`] -> [`KeyAction`].
//!
//! Keeping the binding table here — out of the event loop in
//! [`crate::tui::run`] — is what makes key handling unit-testable
//! without a live terminal. [`map_key`] is a total function: every
//! `KeyEvent` maps to some `KeyAction` (falling back to
//! [`KeyAction::None`]), so it can never panic on an unexpected key.

use super::app::Focus;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// A decoded user intent, independent of which physical key produced it.
///
/// The event loop matches on this rather than on raw key codes so the
/// dispatch logic and the binding table stay separately testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Quit the TUI.
    Quit,
    /// Move focus to the next pane.
    FocusNext,
    /// Execute the current query buffer against the connection.
    RunQuery,
    /// Scroll the results pane up one line.
    ScrollUp,
    /// Scroll the results pane down one line.
    ScrollDown,
    /// Scroll the results pane up one page.
    PageUp,
    /// Scroll the results pane down one page.
    PageDown,
    /// Move the schema-tree selection up one visible row.
    TreeUp,
    /// Move the schema-tree selection down one visible row.
    TreeDown,
    /// Expand or collapse the selected schema-tree node.
    ToggleExpand,
    /// Insert a literal character into the focused input.
    Char(char),
    /// Delete the character before the input cursor.
    Backspace,
    /// Delete the character at the input cursor.
    Delete,
    /// Move the input cursor one character left.
    CursorLeft,
    /// Move the input cursor one character right.
    CursorRight,
    /// Move the input cursor to the start of the line.
    CursorHome,
    /// Move the input cursor to the end of the line.
    CursorEnd,
    /// No bound action for this key.
    None,
}

/// Map a crossterm [`KeyEvent`] to a [`KeyAction`] given the focused
/// pane.
///
/// Only `Press` (and the terminal-default [`KeyEventKind::Repeat`])
/// events produce actions; key-release events map to
/// [`KeyAction::None`] so a key is never double-handled on terminals
/// that report both edges.
///
/// Global bindings (handled in every pane):
/// - `Ctrl-Q` — [`KeyAction::Quit`].
/// - `Tab` — [`KeyAction::FocusNext`].
///
/// When [`Focus::Input`] is active, printable characters map to
/// [`KeyAction::Char`] and editing keys to their cursor/delete actions;
/// `Ctrl-Enter` runs the query and `Esc` on an input-focused, *empty*
/// buffer is treated as quit by the caller (the buffer-empty check lives
/// in the event loop, not here, so this function stays buffer-agnostic).
#[must_use]
pub fn map_key(key: KeyEvent, focus: Focus) -> KeyAction {
    // Ignore key-release edges; only act on press / repeat.
    if key.kind == KeyEventKind::Release {
        return KeyAction::None;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Global bindings first, regardless of focus.
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('c') if ctrl => return KeyAction::Quit,
        KeyCode::Tab => return KeyAction::FocusNext,
        _ => {}
    }

    match focus {
        Focus::Input => map_input(key, ctrl),
        Focus::Results => map_results(key),
        Focus::SchemaTree => map_tree(key),
    }
}

fn map_input(key: KeyEvent, ctrl: bool) -> KeyAction {
    match key.code {
        // Ctrl-Enter runs the query; plain Enter inserts a newline so a
        // multi-line statement can be typed.
        KeyCode::Enter if ctrl => KeyAction::RunQuery,
        KeyCode::Enter => KeyAction::Char('\n'),
        KeyCode::Char(c) if !ctrl => KeyAction::Char(c),
        KeyCode::Backspace => KeyAction::Backspace,
        KeyCode::Delete => KeyAction::Delete,
        KeyCode::Left => KeyAction::CursorLeft,
        KeyCode::Right => KeyAction::CursorRight,
        KeyCode::Home => KeyAction::CursorHome,
        KeyCode::End => KeyAction::CursorEnd,
        _ => KeyAction::None,
    }
}

fn map_results(key: KeyEvent) -> KeyAction {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => KeyAction::ScrollUp,
        KeyCode::Down | KeyCode::Char('j') => KeyAction::ScrollDown,
        KeyCode::PageUp => KeyAction::PageUp,
        KeyCode::PageDown => KeyAction::PageDown,
        _ => KeyAction::None,
    }
}

fn map_tree(key: KeyEvent) -> KeyAction {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => KeyAction::TreeUp,
        KeyCode::Down | KeyCode::Char('j') => KeyAction::TreeDown,
        KeyCode::Enter | KeyCode::Char(' ') => KeyAction::ToggleExpand,
        _ => KeyAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_q_quits_from_every_focus() {
        for focus in [Focus::SchemaTree, Focus::Input, Focus::Results] {
            assert_eq!(map_key(ctrl(KeyCode::Char('q')), focus), KeyAction::Quit);
        }
    }

    #[test]
    fn tab_cycles_focus_from_every_focus() {
        for focus in [Focus::SchemaTree, Focus::Input, Focus::Results] {
            assert_eq!(map_key(press(KeyCode::Tab), focus), KeyAction::FocusNext);
        }
    }

    #[test]
    fn ctrl_enter_runs_query_when_input_focused() {
        assert_eq!(
            map_key(ctrl(KeyCode::Enter), Focus::Input),
            KeyAction::RunQuery
        );
    }

    #[test]
    fn plain_char_inserts_when_input_focused() {
        assert_eq!(
            map_key(press(KeyCode::Char('x')), Focus::Input),
            KeyAction::Char('x')
        );
    }

    #[test]
    fn plain_enter_inserts_newline_when_input_focused() {
        assert_eq!(
            map_key(press(KeyCode::Enter), Focus::Input),
            KeyAction::Char('\n')
        );
    }

    #[test]
    fn arrow_keys_scroll_results_when_results_focused() {
        assert_eq!(
            map_key(press(KeyCode::Up), Focus::Results),
            KeyAction::ScrollUp
        );
        assert_eq!(
            map_key(press(KeyCode::Down), Focus::Results),
            KeyAction::ScrollDown
        );
    }

    #[test]
    fn enter_toggles_expand_when_tree_focused() {
        assert_eq!(
            map_key(press(KeyCode::Enter), Focus::SchemaTree),
            KeyAction::ToggleExpand
        );
    }

    #[test]
    fn unmapped_key_is_none() {
        // F5 has no binding in any pane.
        assert_eq!(
            map_key(press(KeyCode::F(5)), Focus::Results),
            KeyAction::None
        );
    }

    #[test]
    fn release_events_are_ignored() {
        let mut ev = ctrl(KeyCode::Char('q'));
        ev.kind = KeyEventKind::Release;
        assert_eq!(map_key(ev, Focus::Input), KeyAction::None);
    }

    #[test]
    fn map_key_never_panics_across_a_keycode_sweep() {
        let focuses = [Focus::SchemaTree, Focus::Input, Focus::Results];
        let mods = [
            KeyModifiers::NONE,
            KeyModifiers::CONTROL,
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
        ];
        let mut codes = vec![
            KeyCode::Backspace,
            KeyCode::Enter,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Delete,
            KeyCode::Insert,
            KeyCode::Null,
            KeyCode::Esc,
            KeyCode::CapsLock,
        ];
        for c in ' '..='~' {
            codes.push(KeyCode::Char(c));
        }
        for f in 1..=12 {
            codes.push(KeyCode::F(f));
        }
        for &focus in &focuses {
            for &m in &mods {
                for &code in &codes {
                    // Just exercising the mapping; the assertion is that
                    // it returns without panicking.
                    let _ = map_key(KeyEvent::new(code, m), focus);
                }
            }
        }
    }
}
