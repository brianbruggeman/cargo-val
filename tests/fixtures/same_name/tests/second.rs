use std::fs::OpenOptions;
use std::path::PathBuf;

// opening a fifo for writing blocks until a reader opens it, so this test finishes only while `first::shared_name` is live
#[test]
fn shared_name() {
    let Some(directory) = std::env::var_os("CARGO_VAL_RENDEZVOUS_DIR") else {
        return;
    };
    OpenOptions::new()
        .write(true)
        .open(PathBuf::from(directory).join("rendezvous"))
        .expect("the rendezvous fifo opens once the other binary's test is live");
}
