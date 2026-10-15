fn main() {
    let argument = std::env::args().nth(1).unwrap_or_default();
    println!("{}", cargo_val_binary_count_fixture::checksum(argument.as_bytes()));
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_binary_harness_has_a_test_of_its_own() {
        assert_eq!(cargo_val_binary_count_fixture::checksum(b""), 0);
    }
}
