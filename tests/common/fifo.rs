use core::time::Duration;

use cargo_val::edge_stream_unix::ChildStream;
use proxima_core::time::timeout;
use proxima_primitives::pipe::{Exhausted, Pipe};

// the fifo has no child to kill; every caller owns its child through supervised or a Drop guard that kills the group when this panics
const FIFO_DEADLINE: Duration = Duration::from_secs(240);

pub async fn first_line(stream: &ChildStream, what: &str) -> String {
    timeout(FIFO_DEADLINE, read_line(stream, what))
        .await
        .unwrap_or_else(|elapsed| panic!("{what}: no line within {FIFO_DEADLINE:?} ({elapsed:?})"))
}

async fn read_line(stream: &ChildStream, what: &str) -> String {
    let mut line = Vec::new();
    while !line.contains(&b'\n') {
        match stream.call(()).await {
            Ok(Ok(range)) => line.extend_from_slice(&stream.window(range).expect("the range was just read")),
            Ok(Err(fault)) => panic!("{what}: the fifo failed to read: {fault}"),
            Err(Exhausted) => break,
        }
    }
    String::from_utf8(line).expect("the fifo carries utf-8")
}

pub async fn until_eof(stream: &ChildStream, what: &str) {
    let read = timeout(FIFO_DEADLINE, stream.call(()))
        .await
        .unwrap_or_else(|elapsed| panic!("{what}: no EOF within {FIFO_DEADLINE:?} ({elapsed:?})"));
    match read {
        Err(Exhausted) => {}
        Ok(read) => panic!("{what}: the fifo carried {read:?} instead of EOF"),
    }
}
