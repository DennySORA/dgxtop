//! Unicode-width-aware fitting of untrusted text into terminal columns.

use dgxtop_core::text::sanitize_display;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Marker appended when text is cut to fit.
const ELLIPSIS: char = '…';
/// Upper bound for any single projected string drawn by the viewer.
const MAX_FIELD_BYTES: usize = 512;

/// Strips control/escape/bidi characters and bounds the byte length. Driver
/// labels and process names are untrusted even after the Manager cleaned
/// them; the viewer re-applies the core hygiene before drawing.
pub fn clean(text: &str) -> String {
    sanitize_display(text, MAX_FIELD_BYTES)
}

/// Display width in terminal columns (CJK is 2 columns wide).
pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Cuts `text` to at most `max_width` columns, ending with `…` when cut.
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    if display_width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width - 1;
    let mut used = 0usize;
    let mut output = String::with_capacity(text.len().min(max_width * 4));
    for character in text.chars() {
        let width = character.width().unwrap_or(0);
        if used + width > budget {
            break;
        }
        used += width;
        output.push(character);
    }
    output.push(ELLIPSIS);
    output
}

/// Left-aligned, truncated and space-padded to exactly `width` columns.
pub fn pad_right(text: &str, width: usize) -> String {
    let fitted = truncate_to_width(text, width);
    let padding = width.saturating_sub(display_width(&fitted));
    format!("{fitted}{}", " ".repeat(padding))
}

/// Right-aligned, truncated and space-padded to exactly `width` columns.
pub fn pad_left(text: &str, width: usize) -> String {
    let fitted = truncate_to_width(text, width);
    let padding = width.saturating_sub(display_width(&fitted));
    format!("{}{fitted}", " ".repeat(padding))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_display_width() {
        assert_eq!(truncate_to_width("python3", 10), "python3");
        assert_eq!(truncate_to_width("python3-worker", 8), "python3…");
        assert_eq!(truncate_to_width("溫度溫度", 5), "溫度…");
        assert_eq!(display_width(&truncate_to_width("溫度溫度", 4)), 3);
        assert_eq!(truncate_to_width("abc", 0), "");
        assert_eq!(truncate_to_width("abc", 1), "…");
    }

    #[test]
    fn clean_removes_terminal_injection() {
        assert_eq!(clean("ok\u{1b}[2Jname\u{202e}"), "okname");
        assert!(clean(&"x".repeat(4096)).len() <= MAX_FIELD_BYTES);
    }
}
