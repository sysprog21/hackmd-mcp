//! The body hash: one format, `sha256:` and 64 lowercase hex digits,
//! reported by `hackmd_get_note` as `body_hash`, taken back as
//! `expected_hash`, and recorded in sync state. It is part of the tool
//! contract, so it lives on its own rather than inside either user.

use sha2::{Digest, Sha256};

/// The single hash format written to sidecars and reported by the sync tools.
pub(crate) fn body_hash(body: &str) -> String {
    body_hash_from_digest(&body_digest(body))
}

/// Whether `value` is in the one format `body_hash` writes: `sha256:` and 64
/// lowercase hex digits. A caller-supplied hash in any other shape can never
/// match, and is better refused as input than reported as a conflict.
fn is_body_hash(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// Whether a caller supplied a hash that `body_hash` could never have
/// written.
pub(crate) fn is_malformed(hash: Option<&str>) -> bool {
    hash.is_some_and(|hash| !is_body_hash(hash))
}

pub(crate) fn body_hash_from_digest(digest: &[u8; 32]) -> String {
    let mut hash = String::with_capacity(71);
    hash.push_str("sha256:");
    push_hex(&mut hash, digest);
    hash
}

pub(crate) fn push_hex(out: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
}

pub(crate) fn body_digest(body: &str) -> [u8; 32] {
    Sha256::digest(body.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    #[test]
    fn hex_encoding_appends_every_byte_as_two_lowercase_digits() {
        let prefix = "prefix:";
        let mut encoded = String::from(prefix);
        let bytes = (0..=u8::MAX).collect::<Vec<_>>();
        super::push_hex(&mut encoded, &bytes);
        assert!(encoded.starts_with(prefix));
        assert_eq!(encoded.len(), prefix.len() + 256 * 2);
        for (hex, byte) in encoded.as_bytes()[prefix.len()..]
            .as_chunks::<2>()
            .0
            .iter()
            .zip(bytes)
        {
            assert_eq!(hex, format!("{byte:02x}").as_bytes());
        }
        assert_eq!(
            super::body_hash(""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn only_the_format_body_hash_writes_is_a_body_hash() {
        assert!(super::is_body_hash(&super::body_hash("any body")));
        for value in [
            "",
            "sha256:",
            "sha256:x",
            &format!("SHA256:{}", "0".repeat(64)),
            &format!("sha256:{}", "A".repeat(64)),
            &format!("sha256:{}", "0".repeat(63)),
            &format!(" sha256:{}", "0".repeat(64)),
        ] {
            assert!(!super::is_body_hash(value), "{value:?}");
        }
    }
}
