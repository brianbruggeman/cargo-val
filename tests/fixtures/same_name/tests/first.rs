use std::fs::File;
use std::path::PathBuf;

// opening a fifo for reading blocks until a writer opens it, so this test finishes only while `second::shared_name` is live
#[test]
fn shared_name() {
    let Some(directory) = std::env::var_os("CARGO_VAL_RENDEZVOUS_DIR") else {
        return;
    };
    File::open(PathBuf::from(directory).join("rendezvous"))
        .expect("the rendezvous fifo opens once the other binary's test is live");
}
