//! Walks the hang X-ray: parse a real `sample` capture into classified frames, then capture this process live.
//!
//! `frames` is a sans-IO iterator (bytes in, borrowed frames out); `edge_stack::capture` is the one OS-choosing edge.

use cargo_val::edge_stack::{FORMAT, capture};
use cargo_val::stack_frames::{Class, Format, frames};

const SAMPLE: &[u8] = include_bytes!("../tests/fixtures/stack/sample_blocked_fixture.txt");
const WORKSPACE: [&str; 1] = ["cargo_val_blocked_fixture"];

fn print_frames(bytes: &[u8], format: Format) {
    let parsed = frames(bytes, format, &WORKSPACE).expect("captured stacks are text");
    let mut thread = "";
    let mut folded = 0;
    for frame in parsed {
        if frame.thread != thread {
            thread = frame.thread;
            println!("{thread}");
        }
        if frame.class == Class::Runtime {
            folded += 1;
            continue;
        }
        if folded > 0 {
            println!("    ... {folded} runtime frames");
            folded = 0;
        }
        println!("    {:?} {} {}:{}", frame.class, frame.symbol, frame.file.unwrap_or("-"), frame.line.unwrap_or(0));
    }
    if folded > 0 {
        println!("    ... {folded} runtime frames");
    }
}

fn main() {
    println!("-- committed real sample capture, runtime frames folded --");
    print_frames(SAMPLE, Format::Sample);
    let user = frames(SAMPLE, Format::Sample, &WORKSPACE).expect("sample output is text").filter(|frame| frame.class == Class::User).count();
    println!("user frames in the vector: {user}");

    println!("-- live capture of this process --");
    let mut buffer = vec![0_u8; 32 * 1024];
    match capture(std::process::id(), &mut buffer) {
        Ok((written, dropped)) => {
            println!("captured {written} bytes, dropped {dropped}");
            print_frames(&buffer[..written], FORMAT);
        }
        Err(reason) => println!("{reason}"),
    }
}
