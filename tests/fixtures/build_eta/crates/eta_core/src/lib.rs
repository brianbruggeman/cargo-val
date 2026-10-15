pub fn split_verb(line: &str) -> Option<(&str, &str)> {
    line.split_once(' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_line_splits_at_the_first_space() {
        assert_eq!(split_verb("GET /a b"), Some(("GET", "/a b")));
    }
}
