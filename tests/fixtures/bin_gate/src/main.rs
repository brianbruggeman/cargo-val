// rustc reads this fifo while compiling the bin, so the bin artifact cannot exist until the harness writes to it
const HOLD: &str = include_str!(concat!(env!("CARGO_VAL_BIN_GATE_HOLD"), ""));

fn main() {
    std::process::exit(i32::from(!HOLD.is_empty()));
}
