use std::io::Write;

const CHUNK_BYTES: usize = 4096;

fn pattern_byte(index: usize) -> u8 {
    if index % 64 == 63 { b'\n' } else { b'a' + u8::try_from(index % 26).unwrap_or(0) }
}

fn write_pattern(mut sink: impl Write, total: usize) {
    let mut written = 0;
    while written < total {
        let chunk: Vec<u8> = (written..total.min(written + CHUNK_BYTES)).map(pattern_byte).collect();
        sink.write_all(&chunk).expect("the pattern chunk is written");
        written += chunk.len();
    }
    sink.flush().expect("the pattern is flushed");
}

pub fn write_from_env() {
    if let Some(total) = std::env::var("CARGO_VAL_VIEWS_BYTES").ok().and_then(|value| value.parse().ok()) {
        write_pattern(std::io::stdout(), total);
        write_pattern(std::io::stderr(), total);
    }
}
