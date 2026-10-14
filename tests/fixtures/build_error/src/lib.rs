pub fn answer() -> u32 {
    /// scale applied before the answer is reported
    let scale = 6;
    "forty-two"
}

#[cfg(test)]
mod tests {
    #[test]
    fn answer_is_forty_two() {
        assert_eq!(super::answer(), 42);
    }
}
