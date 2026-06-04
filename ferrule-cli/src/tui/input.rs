//! Query input buffer for the TUI editor pane.
//!
//! A single logical text buffer with a character cursor. Newlines are
//! permitted so a query can span several visual lines; there is no
//! syntax highlighting and no autocompletion in this increment (both
//! are deferred — see [`crate::tui`]).
//!
//! The cursor is tracked as a **character** index (not a byte index) so
//! editing stays correct for multi-byte UTF-8 input. Everything here is
//! pure and side-effect free, which is what makes it unit-testable
//! without a terminal.

/// The editable query buffer plus a character cursor.
#[derive(Debug, Default, Clone)]
pub struct InputBuffer {
    /// The full query text.
    text: String,
    /// Cursor position as a character offset in `0..=char_count`.
    cursor: usize,
}

impl InputBuffer {
    /// An empty buffer with the cursor at the start.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current buffer contents.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cursor position as a character offset.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Number of characters (not bytes) in the buffer.
    #[must_use]
    pub fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// `true` when the buffer holds no text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Byte offset of the current character cursor, for slicing `text`.
    fn byte_offset(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor)
            .map_or(self.text.len(), |(b, _)| b)
    }

    /// Insert `ch` at the cursor and advance the cursor by one.
    pub fn insert_char(&mut self, ch: char) {
        let at = self.byte_offset();
        self.text.insert(at, ch);
        self.cursor += 1;
    }

    /// Delete the character before the cursor (Backspace). No-op at the
    /// start of the buffer.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let at = self.byte_offset();
        self.text.remove(at);
    }

    /// Delete the character at the cursor (Delete). No-op at end of
    /// buffer.
    pub fn delete(&mut self) {
        if self.cursor >= self.char_count() {
            return;
        }
        let at = self.byte_offset();
        self.text.remove(at);
    }

    /// Move the cursor one character left. No-op at the start.
    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the cursor one character right. No-op at the end.
    pub fn move_right(&mut self) {
        if self.cursor < self.char_count() {
            self.cursor += 1;
        }
    }

    /// Move the cursor to the start of the buffer.
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the end of the buffer.
    pub fn move_end(&mut self) {
        self.cursor = self.char_count();
    }

    /// Replace the entire buffer and place the cursor at the end.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.char_count();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_char_advances_cursor_and_grows_buffer() {
        let mut b = InputBuffer::new();
        b.insert_char('S');
        b.insert_char('Q');
        b.insert_char('L');
        assert_eq!(b.text(), "SQL");
        assert_eq!(b.cursor(), 3);
        assert_eq!(b.char_count(), 3);
    }

    #[test]
    fn backspace_at_position_zero_is_noop() {
        let mut b = InputBuffer::new();
        b.backspace();
        assert_eq!(b.text(), "");
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn move_home_and_move_end_set_cursor_to_bounds() {
        let mut b = InputBuffer::new();
        b.set_text("select 1");
        assert_eq!(b.cursor(), 8);
        b.move_home();
        assert_eq!(b.cursor(), 0);
        b.move_end();
        assert_eq!(b.cursor(), 8);
    }

    #[test]
    fn delete_at_end_is_noop() {
        let mut b = InputBuffer::new();
        b.set_text("ab");
        b.move_end();
        b.delete();
        assert_eq!(b.text(), "ab");
    }

    #[test]
    fn delete_at_cursor_removes_following_char() {
        let mut b = InputBuffer::new();
        b.set_text("abc");
        b.move_home();
        b.move_right();
        b.delete();
        assert_eq!(b.text(), "ac");
        assert_eq!(b.cursor(), 1);
    }

    #[test]
    fn insert_then_backspace_round_trips_to_original() {
        let mut b = InputBuffer::new();
        b.set_text("base");
        let original = b.text().to_string();
        b.insert_char('X');
        assert_eq!(b.text(), "baseX");
        b.backspace();
        assert_eq!(b.text(), original);
        assert_eq!(b.cursor(), 4);
    }

    #[test]
    fn move_left_clamps_at_zero_move_right_clamps_at_end() {
        let mut b = InputBuffer::new();
        b.set_text("hi");
        b.move_home();
        b.move_left();
        assert_eq!(b.cursor(), 0);
        b.move_end();
        b.move_right();
        assert_eq!(b.cursor(), 2);
    }

    #[test]
    fn editing_is_correct_across_multibyte_chars() {
        let mut b = InputBuffer::new();
        // "café" — the é is two bytes.
        for ch in "café".chars() {
            b.insert_char(ch);
        }
        assert_eq!(b.char_count(), 4);
        b.backspace();
        assert_eq!(b.text(), "caf");
        // Insert in the middle, before the 'a'.
        b.move_home();
        b.move_right();
        b.insert_char('Z');
        assert_eq!(b.text(), "cZaf");
    }
}
