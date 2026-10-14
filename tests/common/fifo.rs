use cargo_val::edge_stream_unix::ChildStream;
use proxima_primitives::pipe::{Exhausted, Pipe};

pub async fn first_line(stream: &ChildStream, what: &str) -> String {
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
    match stream.call(()).await {
        Err(Exhausted) => {}
        Ok(read) => panic!("{what}: the fifo carried {read:?} instead of EOF"),
    }
}
