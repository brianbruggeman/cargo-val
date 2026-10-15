use eta_text::path_of;

pub fn route(line: &str) -> String {
    path_of(line).map_or_else(|| "rejected".to_string(), |path| path.replace('/', ":"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_segments_are_joined_with_colons() {
        assert_eq!(route("GET /users/42"), "users:42");
    }
}
