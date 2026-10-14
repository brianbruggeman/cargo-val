//! Walks the hang X-ray: parse a real `sample` capture into folded rows, then capture this process live.
//!
//! `Rows` is a sans-IO iterator (bytes in, borrowed rows out); `edge_stack::capture` is the one OS-choosing edge.

use cargo_val::edge_stack::capture;
use cargo_val::stack_capture::Capture;
use cargo_val::stack_frames::{Class, Format, Row, Rows};

const SAMPLE: &[u8] = include_bytes!("../tests/fixtures/stack/sample_blocked_fixture.txt");
const WORKSPACE: [&str; 1] = ["cargo_val_blocked_fixture"];

fn print_rows(bytes: &[u8], format: Format) {
    let rows = Rows::folded(bytes, format, &WORKSPACE).expect("captured stacks are text");
    for row in rows {
        match row {
            Row::Thread(thread) => println!("{thread}"),
            Row::Folded(count) => println!("    ... {count} runtime frames"),
            Row::Frame(frame) => println!(
                "    {:?} {} {}:{}",
                frame.class,
                frame.symbol,
                frame.file.unwrap_or("-"),
                frame.line.unwrap_or(0)
            ),
        }
    }
}

fn main() {
    println!("-- committed real sample capture, folded --");
    print_rows(SAMPLE, Format::Sample);
    let user = Rows::raw(SAMPLE, Format::Sample, &WORKSPACE)
        .expect("sample output is text")
        .filter(|row| matches!(row, Row::Frame(frame) if frame.class == Class::User))
        .count();
    println!("user frames in the vector: {user}");

    println!("-- live capture of this process --");
    let mut buffer = vec![0_u8; 32 * 1024];
    match capture(std::process::id(), &mut buffer) {
        Capture::Captured { format, written, dropped, source } => {
            println!("captured {written} bytes, dropped {dropped}, tool produced {source}");
            print_rows(&buffer[..written], format);
        }
        Capture::Unavailable(reason) => println!("{reason}"),
    }
}
