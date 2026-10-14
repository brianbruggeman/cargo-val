fn main() {
    println!("cargo::warning=build script warning that cargo prints to its own stderr");
    eprintln!("build script stderr line written while the rich frame is live");
}
