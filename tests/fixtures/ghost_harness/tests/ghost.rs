fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    if arguments.iter().any(|argument| argument == "--list") {
        if !arguments.iter().any(|argument| argument == "--ignored") {
            println!("ghost::vanishes: test\n\n1 test, 0 benchmarks");
        }
        return;
    }
    println!("running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s");
}
