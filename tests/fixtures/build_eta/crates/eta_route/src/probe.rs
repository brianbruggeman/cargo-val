fn main() {
    let line = std::env::args().nth(1).unwrap_or_else(|| "GET /health".to_string());
    println!("{}", eta_route::route(&line));
}
