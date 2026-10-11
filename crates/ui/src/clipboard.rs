//! Copying text to the terminal's clipboard (D-76) with OSC 52, which most
//! terminals (and tmux with `set-clipboard on`) honour; a terminal that
//! does not ignores it. Nothing is read back.

/// Text copied at most; terminals refuse longer OSC 52 payloads.
pub const MAX_COPY_BYTES: usize = 8192;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `ESC ] 52 ; c ; <base64> BEL` setting the clipboard to `text`, cut at
/// [`MAX_COPY_BYTES`] on a character boundary.
pub fn osc52(text: &str) -> Vec<u8> {
    let mut end = text.len().min(MAX_COPY_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut sequence = b"\x1b]52;c;".to_vec();
    sequence.extend(base64(&text.as_bytes()[..end]));
    sequence.push(0x07);
    sequence
}

fn base64(bytes: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let joined = chunk.iter().enumerate().fold(0u32, |word, (index, byte)| {
            word | (u32::from(*byte) << (16 - 8 * index))
        });
        for index in 0..4 {
            if index <= chunk.len() {
                let sextet = (joined >> (18 - 6 * index)) & 0x3f;
                encoded.push(ALPHABET[sextet as usize]);
            } else {
                encoded.push(b'=');
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), b"");
        assert_eq!(base64(b"f"), b"Zg==");
        assert_eq!(base64(b"fo"), b"Zm8=");
        assert_eq!(base64(b"foo"), b"Zm9v");
        assert_eq!(base64(b"python -m acme"), b"cHl0aG9uIC1tIGFjbWU=");
    }

    #[test]
    fn osc52_wraps_the_text_and_bounds_it() {
        assert_eq!(osc52("hi"), b"\x1b]52;c;aGk=\x07");
        let long = "é".repeat(MAX_COPY_BYTES);
        let sequence = osc52(&long);
        let payload = sequence.len() - b"\x1b]52;c;\x07".len();
        assert!(payload <= MAX_COPY_BYTES.div_ceil(3) * 4);
    }
}
