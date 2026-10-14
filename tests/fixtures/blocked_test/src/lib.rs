#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
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
        // opened before the pid is announced so the harness always finds a holder: a nonblocking write open of a fifo nobody holds returns ENXIO
        let mut block = OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.join("block"))
            .expect("the block fifo opens read-write without a peer");
        let mut ready = OpenOptions::new()
            .write(true)
            .open(directory.join("ready"))
            .expect("the harness opened the ready fifo for reading");
        ready
            .write_all(format!("{}\n", std::process::id()).as_bytes())
            .expect("the ready fifo accepts the process id");
        drop(ready);
        let _ = block.read(&mut [0_u8; 1]);
    }
}
