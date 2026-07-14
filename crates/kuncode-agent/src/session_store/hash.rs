//! Canonical digests for context snapshots crossing durable commit boundaries.

use kuncode_core::completion::Message;
use sha2::{Digest, Sha256};

use super::{SessionStoreError, dto};

/// Hashes the UTF-8 bytes of the versioned, provider-neutral store encoding.
///
/// The result is a bare 64-character lowercase SHA-256 digest with no algorithm prefix.
pub(crate) fn active_messages_sha256(messages: &[Message]) -> Result<String, SessionStoreError> {
    let encoded = dto::messages_to_string(messages)?;
    Ok(format!("{:x}", Sha256::digest(encoded.as_bytes())))
}

pub(crate) fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use kuncode_core::completion::Message;

    use super::active_messages_sha256;

    #[test]
    fn active_message_hash_uses_stable_versioned_store_encoding() {
        let messages = [Message::user("summary")];

        let digest = active_messages_sha256(&messages).expect("messages should encode");

        assert_eq!(
            digest,
            "7adc41e95596394331fab4d5671cd662e2220dd5d567c285ccaab7d6845e5548"
        );
    }
}
