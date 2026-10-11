#[cfg(test)]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;

    // the held fifo's write end stays open until this process exits, so the harness reads EOF exactly then
    #[test]
    fn unit_blocks_when_requested() {
        let Some(directory) = std::env::var_os("CARGO_VAL_FIXTURE_TEST_BLOCK_DIR") else {
            return;
        };
        let directory = PathBuf::from(directory);
        let _held = OpenOptions::new()
            .write(true)
            .open(directory.join("held"))
            .expect("the harness opened the held fifo for reading");
        let mut ready = OpenOptions::new()
            .write(true)
            .open(directory.join("ready"))
            .expect("the harness opened the ready fifo for reading");
        ready
            .write_all(format!("{}\n", std::process::id()).as_bytes())
            .expect("the ready fifo accepts the process id");
        drop(ready);
        let _ = File::open(directory.join("block"));
    }
}
