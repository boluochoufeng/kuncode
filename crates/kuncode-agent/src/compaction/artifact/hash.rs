use sha2::{Digest, Sha256};

pub(super) fn sha256_hex(input: &[u8]) -> String {
    format!("{:x}", Sha256::digest(input))
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
