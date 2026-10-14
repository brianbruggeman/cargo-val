#[cfg(test)]
mod fixture {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};

    const CHUNK_BYTES: usize = 4096;

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

    fn fail_with_output(variable: &str, seed: usize) {
        let Some(total) = std::env::var(variable).ok().and_then(|value| value.parse().ok()) else {
            return;
        };
        write_pattern(std::io::stdout(), seed, total);
        write_pattern(std::io::stderr(), seed, total);
        panic!("views_flood failure {seed}");
    }

    // a settler waits for the harness to release it, then reports that it ran; with no fifos it just passes
    fn settle() {
        let (Some(go), Some(done)) = (std::env::var_os("VIEWS_FLOOD_GO"), std::env::var_os("VIEWS_FLOOD_DONE")) else {
            return;
        };
        let mut released = [0_u8; 1];
        OpenOptions::new().read(true).open(go).expect("the go fifo opens").read_exact(&mut released).expect("the harness releases the settler");
        OpenOptions::new().write(true).open(done).expect("the done fifo opens").write_all(b"x").expect("the settler reports it ran");
    }

    #[test]
    fn fails_big() {
        fail_with_output("VIEWS_FLOOD_BIG_BYTES", 3);
    }

    include!(concat!(env!("OUT_DIR"), "/tests.rs"));
}
