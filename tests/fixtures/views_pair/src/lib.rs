#[cfg(test)]
mod fixture {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};

    const CHUNK_BYTES: usize = 4096;
    const PRODUCER_BYTES: usize = 4 * 1024 * 1024;

    fn pattern_byte(seed: usize, index: usize) -> u8 {
        if index % 64 == 63 { b'\n' } else { b'a' + u8::try_from((index + seed) % 26).unwrap_or(0) }
    }

    fn write_pattern(mut sink: impl Write, seed: usize, total: usize) {
        let mut written = 0;
        while written < total {
            let chunk: Vec<u8> = (written..total.min(written + CHUNK_BYTES)).map(|index| pattern_byte(seed, index)).collect();
            sink.write_all(&chunk).expect("the pattern chunk is written");
            written += chunk.len();
        }
        sink.flush().expect("the pattern is flushed");
    }

    fn bytes_from(variable: &str) -> Option<usize> {
        std::env::var(variable).ok().and_then(|value| value.parse().ok())
    }

    fn fail_with_output(variable: &str, seed: usize) {
        if let Some(total) = bytes_from(variable) {
            write_pattern(std::io::stdout(), seed, total);
            write_pattern(std::io::stderr(), seed, total);
            panic!("views_pair failure {seed}");
        }
    }

    #[test]
    fn fails_a() {
        fail_with_output("VIEWS_PAIR_BYTES", 0);
    }

    #[test]
    fn fails_b() {
        fail_with_output("VIEWS_PAIR_BYTES", 7);
    }

    #[test]
    fn fails_big() {
        fail_with_output("VIEWS_PAIR_BIG_BYTES", 3);
    }

    #[test]
    fn producer() {
        let (Some(go), Some(done)) = (std::env::var_os("VIEWS_PAIR_GO"), std::env::var_os("VIEWS_PAIR_DONE")) else {
            return;
        };
        let mut released = [0_u8; 1];
        OpenOptions::new().read(true).open(go).expect("the go fifo opens").read_exact(&mut released).expect("the harness releases the producer");
        write_pattern(std::io::stdout(), 5, PRODUCER_BYTES);
        OpenOptions::new().write(true).open(done).expect("the done fifo opens").write_all(b"x").expect("the producer reports it finished writing");
    }
}
