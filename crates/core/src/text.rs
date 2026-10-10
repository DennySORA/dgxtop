//! Text hygiene for anything that reaches a terminal, a log or an audit row.
//!
//! Driver labels, process names and error strings are untrusted: they may carry
//! ANSI/OSC escape sequences or unbounded length. Every displayed or persisted
//! string goes through [`sanitize_display`] so terminal injection is impossible
//! and byte budgets hold.

/// Maximum bytes of a sanitized process basename.
pub const MAX_BASENAME_BYTES: usize = 64;
/// Maximum bytes of a sanitized error/diagnostic message.
pub const MAX_MESSAGE_BYTES: usize = 1024;

/// Removes control characters (C0, DEL, C1) and escape sequences, then cuts the
/// result at a UTF-8 boundary so it never exceeds `max_bytes`.
///
/// Tabs and newlines become a single space because display rows are single line.
pub fn sanitize_display(input: &str, max_bytes: usize) -> String {
    let mut output = String::with_capacity(input.len().min(max_bytes));
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            skip_escape_sequence(&mut characters);
            continue;
        }
        let replacement = match character {
            '\t' | '\n' | '\r' => Some(' '),
            c if c.is_control() => None,
            c if is_bidi_override(c) => None,
            c => Some(c),
        };
        let Some(kept) = replacement else {
            continue;
        };
        if output.len() + kept.len_utf8() > max_bytes {
            break;
        }
        output.push(kept);
    }
    output
}

/// Returns the final path component of a command or executable path, sanitized.
pub fn sanitize_basename(path_or_comm: &str) -> String {
    let trimmed = path_or_comm.trim_end_matches('/');
    let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let cleaned = sanitize_display(base, MAX_BASENAME_BYTES);
    if cleaned.is_empty() {
        "?".to_owned()
    } else {
        cleaned
    }
}

/// Sanitizes a kernel command name (`comm`). Unlike a path it may contain
/// `/` (e.g. `kworker/0:1H`), so nothing is split; empty names become `?`.
pub fn sanitize_command_name(comm: &str) -> String {
    let cleaned = sanitize_display(comm.trim(), MAX_BASENAME_BYTES);
    if cleaned.is_empty() {
        "?".to_owned()
    } else {
        cleaned
    }
}

/// Bounded, auditable form of untrusted text for persistent records
/// (`contracts/INTERFACES.md`: displays strip control codes, the Store keeps
/// them escaped). Control characters, DEL, C1 and bidi overrides become
/// `\u{XX}`; a literal backslash becomes `\\`, so the form is unambiguous.
/// The result never exceeds `max_bytes` and never ends inside an escape.
pub fn escape_for_audit(input: &str, max_bytes: usize) -> String {
    let mut output = String::with_capacity(input.len().min(max_bytes));
    for character in input.chars() {
        let piece = match character {
            '\\' => r"\\".to_owned(),
            c if c.is_control() || is_bidi_override(c) => format!(r"\u{{{:x}}}", u32::from(c)),
            c => c.to_string(),
        };
        if output.len() + piece.len() > max_bytes {
            break;
        }
        output.push_str(&piece);
    }
    output
}

/// Command name for persistent records: an absolute path is reduced to its
/// final component (privacy: a full path is never stored), a kernel command
/// name keeps its slashes (`kworker/0:1H`), and the result is escaped and
/// bounded by [`escape_for_audit`].
pub fn audit_command_name(comm_or_path: &str) -> String {
    let name = if comm_or_path.starts_with('/') {
        let trimmed = comm_or_path.trim_end_matches('/');
        trimmed.rsplit('/').next().unwrap_or(trimmed)
    } else {
        comm_or_path
    };
    escape_for_audit(name, MAX_BASENAME_BYTES)
}

/// Sanitizes a free-form message to the shared message limit.
pub fn sanitize_message(message: &str) -> String {
    sanitize_display(message, MAX_MESSAGE_BYTES)
}

/// Truncates at a UTF-8 boundary without other rewriting (for already clean text).
pub fn truncate_utf8(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

fn is_bidi_override(character: char) -> bool {
    matches!(
        character,
        '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}'
    )
}

/// Consumes the remainder of an escape sequence after ESC (CSI, OSC, DCS, or a
/// two-character escape). Unterminated sequences consume the rest of the input.
fn skip_escape_sequence(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match characters.peek().copied() {
        Some('[') => {
            characters.next();
            // CSI: parameters/intermediates until a final byte in 0x40..=0x7e.
            for character in characters.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&character) {
                    break;
                }
            }
        }
        Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
            characters.next();
            // OSC/DCS/SOS/PM/APC: until BEL or ST (ESC \).
            while let Some(character) = characters.next() {
                if character == '\u{7}' {
                    break;
                }
                if character == '\u{1b}' && characters.peek() == Some(&'\\') {
                    characters.next();
                    break;
                }
            }
        }
        Some(_) => {
            characters.next();
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_and_osc_sequences() {
        let hostile =
            "ok\u{1b}[31mred\u{1b}[0m\u{1b}]0;title\u{7}end\u{1b}]8;;http://x\u{1b}\\link";
        assert_eq!(sanitize_display(hostile, 256), "okredendlink");
    }

    #[test]
    fn strips_c0_c1_and_bidi_controls() {
        let hostile = "a\u{0}b\u{7f}c\u{85}d\u{202e}e\tf\ng";
        assert_eq!(sanitize_display(hostile, 256), "abcde f g");
    }

    #[test]
    fn audit_escapes_keep_hostile_bytes_visible_and_unambiguous() {
        assert_eq!(escape_for_audit("sleep", 64), "sleep");
        assert_eq!(escape_for_audit("kworker/0:1H", 64), "kworker/0:1H");
        assert_eq!(
            escape_for_audit("evil\u{1b}[31m\u{7}\u{202e}x", 64),
            r"evil\u{1b}[31m\u{7}\u{202e}x"
        );
        assert_eq!(
            escape_for_audit("a\tb\u{85}c\u{7f}d", 64),
            r"a\u{9}b\u{85}c\u{7f}d"
        );
        // A literal backslash is escaped so it never looks like an escape.
        assert_eq!(escape_for_audit(r"a\u{1b}", 64), r"a\\u{1b}");
        assert_eq!(escape_for_audit("溫度", 64), "溫度");
    }

    #[test]
    fn audit_command_names_drop_paths_but_keep_kernel_slashes() {
        assert_eq!(audit_command_name("/opt/conda/bin/python3"), "python3");
        assert_eq!(audit_command_name("kworker/0:1H"), "kworker/0:1H");
        assert_eq!(audit_command_name("evil\u{1b}[2J/x"), r"evil\u{1b}[2J/x");
        assert_eq!(audit_command_name("/usr/bin/"), "bin");
        assert_eq!(audit_command_name("sleep"), "sleep");
    }

    #[test]
    fn audit_escapes_are_bounded_without_splitting_an_escape() {
        // `\u{1b}` needs 6 bytes after "ab": it is left out whole.
        assert_eq!(escape_for_audit("ab\u{1b}cd", 7), "ab");
        assert_eq!(escape_for_audit("ab\u{1b}cd", 8), r"ab\u{1b}");
        assert_eq!(escape_for_audit("溫度溫度", 7), "溫度");
        assert_eq!(escape_for_audit("", 8), "");
    }

    #[test]
    fn truncates_on_utf8_boundary() {
        let text = "溫度溫度";
        let cut = sanitize_display(text, 7);
        assert_eq!(cut, "溫度");
        assert!(cut.len() <= 7);
        assert_eq!(truncate_utf8("溫度", 4), "溫");
    }

    #[test]
    fn unterminated_escape_consumes_rest() {
        assert_eq!(sanitize_display("safe\u{1b}]0;never-ends", 64), "safe");
    }
}
