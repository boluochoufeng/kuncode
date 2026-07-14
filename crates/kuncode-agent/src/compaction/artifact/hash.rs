//! Computes content identities used to bind artifacts and their source results.

use sha2::{Digest, Sha256};

use kuncode_core::completion::ToolResult;

pub(super) fn sha256_hex(input: &[u8]) -> String {
    format!("{:x}", Sha256::digest(input))
}

/// Hashes the complete serialized [`ToolResult`] used by later slimming checks.
///
/// Binding the serde JSON representation includes protocol identifiers and
/// content structure, not only the visible payload text.
///
/// # Errors
/// Returns an error when the complete result cannot be serialized.
pub(crate) fn tool_result_hash(result: &ToolResult) -> Result<String, serde_json::Error> {
    serde_json::to_vec(result).map(|encoded| format!("sha256-{}", sha256_hex(&encoded)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn sha256_is_stable_across_padding_boundaries() {
        for length in [55, 56, 63, 64, 65, 128] {
            let payload = vec![b'x'; length];
            let first = super::sha256_hex(&payload);
            let second = super::sha256_hex(&payload);

            assert_eq!(first, second);
            assert_eq!(first.len(), 64);
            assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn sha256_keeps_content_addressed_ids_distinct() {
        let first = format!("tool-result-sha256-{}", super::sha256_hex(b"first"));
        let second = format!("tool-result-sha256-{}", super::sha256_hex(b"second"));

        assert_ne!(first, second);
        assert_eq!(first.len(), "tool-result-sha256-".len() + 64);
    }
}
