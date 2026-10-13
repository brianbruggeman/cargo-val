use std::io::Write as _;

fn record(name: &str) {
    let Some(path) = std::env::var_os("CARGO_VAL_DONE_LOG") else {
        return;
    };
    let mut line = name.as_bytes().to_vec();
    line.push(b'\n');
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("done log is openable");
    let written = log.write(&line).expect("done log accepts the line");
    assert_eq!(written, line.len(), "a done line is a single write");
}

include!(concat!(env!("OUT_DIR"), "/cases.rs"));
