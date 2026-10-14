use std::io;

use proxima_primitives::pipe::{Exhausted, Pipe};

#[cfg(unix)]
use cargo_val::edge_stream_unix::ChildStream;
#[cfg(windows)]
use cargo_val::edge_stream_windows::ChildStream;

/// Reads `stream` until `done` accepts everything read so far, or to EOF.
pub async fn drain_until(stream: &ChildStream, done: impl Fn(&[u8]) -> bool) -> (Vec<u8>, Option<io::Error>) {
    let mut bytes = Vec::new();
    while !done(&bytes) {
        match stream.call(()).await {
            Ok(Ok(range)) => match stream.window(range) {
                Some(window) => bytes.extend_from_slice(&window),
                None => return (bytes, Some(io::Error::other("a yielded range reached past the capture window"))),
            },
            Ok(Err(fault)) => return (bytes, Some(fault)),
            Err(Exhausted) => break,
        }
    }
    (bytes, None)
}

