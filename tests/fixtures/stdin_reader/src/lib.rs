#[cfg(test)]
mod tests {
    use std::io::Read;

    #[test]
    fn unit_reads_stdin() {
        let mut byte = [0_u8; 1];
        let read = std::io::stdin().read(&mut byte);
        assert!(read.is_ok(), "stdin reads without an error: {read:?}");
    }
}
