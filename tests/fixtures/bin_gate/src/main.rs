// rustc reads this fifo while compiling the bin, so the bin artifact cannot exist until the harness writes to it
const HOLD: &str = include_str!(concat!(env!("CARGO_VAL_BIN_GATE_HOLD"), ""));

// expands after the fifo read above, so a test linked while the bin is held still sees the bin fail
#[cfg(broken_bin)]
compile_error!("the bin is built broken on purpose");

fn main() {
    std::process::exit(i32::from(!HOLD.is_empty()));
}
