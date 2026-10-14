pub fn consume(text: String) -> usize {
    text.len()
}

pub fn twice() -> usize {
    let text = String::from("forty-two");
    let first = consume(text);
    first + consume(text)
}
