pub fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0_u32, |sum, byte| sum.wrapping_mul(31).wrapping_add(u32::from(*byte)))
}

#[cfg(test)]
mod tests {
    use super::checksum;

    #[test]
    fn checksum_of_a_request_line_is_stable() {
        assert_eq!(checksum(b"GET /health HTTP/1.1"), checksum(b"GET /health HTTP/1.1"));
    }
}
