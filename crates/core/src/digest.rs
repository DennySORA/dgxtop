//! SHA-256 helpers used for command payload idempotency and audit digests.

use sha2::{Digest, Sha256};

/// Lower-case hex SHA-256 of `bytes` (64 characters).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(64);
    for byte in digest {
        text.push(hex_digit(byte >> 4));
        text.push(hex_digit(byte & 0x0f));
    }
    text
}

/// Hex SHA-256 of the canonical JSON encoding of a serializable value.
///
/// `serde_json` emits struct fields in declaration order, so identical payloads
/// produce identical digests within one binary version.
pub fn sha256_json<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let encoded = serde_json::to_vec(value)?;
    Ok(sha256_hex(&encoded))
}

fn hex_digit(nibble: u8) -> char {
    char::from(if nibble < 10 {
        b'0' + nibble
    } else {
        b'a' + nibble - 10
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex(b"").len(), 64);
    }
}
