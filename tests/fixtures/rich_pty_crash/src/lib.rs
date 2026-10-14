#[cfg(test)]
mod tests {
    #[test]
    fn dies_without_a_panic_message() {
        std::process::abort();
    }
}
