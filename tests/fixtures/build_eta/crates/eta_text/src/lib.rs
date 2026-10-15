use eta_core::split_verb;

pub fn path_of(line: &str) -> Option<&str> {
    split_verb(line).filter(|(verb, _)| *verb == "GET").map(|(_, path)| path.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_get_lines_yield_a_path() {
        assert_eq!(path_of("GET /health"), Some("health"));
        assert_eq!(path_of("PUT /health"), None);
    }
}
