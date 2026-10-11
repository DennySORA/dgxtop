//! Uncommitted filter text: the Controller owns the edit buffer and cursor
//! until Enter submits it; the authoritative filter lives in the Manager.

use dgxtop_core::commands::MAX_FILTER_BYTES;
use thiserror::Error;

/// Why a character was not inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FilterEditError {
    #[error("filter is limited to {MAX_FILTER_BYTES} bytes")]
    TooLong,
    #[error("control characters are not allowed in a filter")]
    ControlCharacter,
}

/// Filter edit buffer with a byte cursor that is always a char boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterEdit {
    buffer: String,
    cursor: usize,
}

impl FilterEdit {
    /// Starts editing from the current filter (cut to the byte limit).
    pub fn new(initial: &str) -> Self {
        let buffer = dgxtop_core::text::truncate_utf8(initial, MAX_FILTER_BYTES).to_owned();
        let cursor = buffer.len();
        Self { buffer, cursor }
    }

    pub fn text(&self) -> &str {
        &self.buffer
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert(&mut self, character: char) -> Result<(), FilterEditError> {
        if character.is_control() {
            return Err(FilterEditError::ControlCharacter);
        }
        if self.buffer.len() + character.len_utf8() > MAX_FILTER_BYTES {
            return Err(FilterEditError::TooLong);
        }
        self.buffer.insert(self.cursor, character);
        self.cursor += character.len_utf8();
        Ok(())
    }

    pub fn backspace(&mut self) {
        if let Some(previous) = self.buffer[..self.cursor].chars().next_back() {
            self.cursor -= previous.len_utf8();
            self.buffer.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.buffer.len() {
            self.buffer.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if let Some(previous) = self.buffer[..self.cursor].chars().next_back() {
            self.cursor -= previous.len_utf8();
        }
    }

    pub fn right(&mut self) {
        if let Some(next) = self.buffer[self.cursor..].chars().next() {
            self.cursor += next.len_utf8();
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.buffer.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_moves_the_cursor_on_char_boundaries() {
        let mut edit = FilterEdit::new("py");
        assert_eq!(edit.cursor(), 2);
        edit.insert('溫').unwrap();
        edit.left();
        edit.insert('x').unwrap();
        assert_eq!(edit.text(), "pyx溫");
        edit.end();
        edit.backspace();
        assert_eq!(edit.text(), "pyx");
        edit.home();
        edit.delete();
        assert_eq!(edit.text(), "yx");
        edit.right();
        edit.insert('-').unwrap();
        assert_eq!(edit.text(), "y-x");
        edit.home();
        edit.backspace();
        edit.left();
        assert_eq!(edit.text(), "y-x");
    }

    #[test]
    fn the_256_byte_limit_is_enforced_in_utf8_bytes() {
        let mut edit = FilterEdit::new(&"a".repeat(255));
        edit.insert('b').unwrap();
        assert_eq!(edit.text().len(), MAX_FILTER_BYTES);
        assert_eq!(edit.insert('c'), Err(FilterEditError::TooLong));
        let mut multibyte = FilterEdit::new(&"a".repeat(254));
        assert_eq!(multibyte.insert('溫'), Err(FilterEditError::TooLong));
        let long = FilterEdit::new(&"溫".repeat(200));
        assert!(long.text().len() <= MAX_FILTER_BYTES);
        assert!(long.text().is_char_boundary(long.cursor()));
    }

    #[test]
    fn control_characters_are_rejected() {
        let mut edit = FilterEdit::new("");
        assert_eq!(
            edit.insert('\u{1b}'),
            Err(FilterEditError::ControlCharacter)
        );
        assert_eq!(edit.text(), "");
    }
}
