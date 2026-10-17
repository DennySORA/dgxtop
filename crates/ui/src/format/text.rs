//! Unicode-width-aware fitting of untrusted text into terminal columns, and
//! counted nouns.

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

/// Cuts `text` to at most `max_width` columns by taking out its middle, so
/// a path keeps where it starts and the file it ends in: `/home/u/…/bin/
/// python3`. The end keeps the larger half.
pub fn truncate_middle(text: &str, max_width: usize) -> String {
    if display_width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width - 1;
    let head_budget = budget / 2;
    let head = leading(text.chars(), head_budget);
    let mut tail = leading(text.chars().rev(), budget - head_budget);
    tail.reverse();
    let mut output: String = head.into_iter().collect();
    output.push(ELLIPSIS);
    output.extend(tail);
    output
}

/// The first of `characters` that fit `budget` columns.
fn leading(characters: impl Iterator<Item = char>, budget: usize) -> Vec<char> {
    let mut used = 0usize;
    characters
        .take_while(|character| {
            used += character.width().unwrap_or(0);
            used <= budget
        })
        .collect()
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

/// `count` `noun`s: `1 run`, `3 runs`.
pub fn counted(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_count_names_its_noun_in_the_singular_for_one() {
        assert_eq!(counted(0, "run"), "0 runs");
        assert_eq!(counted(1, "ended run"), "1 ended run");
        assert_eq!(counted(256, "panel"), "256 panels");
    }

    #[test]
    fn truncation_respects_display_width() {
        assert_eq!(truncate_to_width("python3", 10), "python3");
        assert_eq!(truncate_to_width("python3-worker", 8), "python3…");
        assert_eq!(truncate_to_width("溫度溫度", 5), "溫度…");
        assert_eq!(display_width(&truncate_to_width("溫度溫度", 4)), 3);
        assert_eq!(truncate_to_width("abc", 0), "");
        assert_eq!(truncate_to_width("abc", 1), "…");
    }

    /// A path cut to fit keeps its start and, with the larger half, the
    /// file it ends in.
    #[test]
    fn a_middle_cut_keeps_both_ends() {
        let path = "/home/user/projects/app/bin/python3";
        assert_eq!(truncate_middle(path, 40), path);
        let cut = truncate_middle(path, 20);
        assert_eq!(cut, "/home/use…in/python3");
        assert_eq!(display_width(&cut), 20);
        assert_eq!(truncate_middle("溫度溫度溫度", 6), "溫…度");
        assert_eq!(truncate_middle("abc", 1), "…");
        assert_eq!(truncate_middle("abc", 0), "");
    }

    #[test]
    fn clean_removes_terminal_injection() {
        assert_eq!(clean("ok\u{1b}[2Jname\u{202e}"), "okname");
        assert!(clean(&"x".repeat(4096)).len() <= MAX_FIELD_BYTES);
    }
}
