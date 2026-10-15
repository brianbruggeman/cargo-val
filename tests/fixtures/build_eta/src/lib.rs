use eta_route::route;

pub fn handle(line: &str) -> String {
    route(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_get_line_is_routed_to_its_path() {
        assert_eq!(handle("GET /users/42"), "users:42");
    }
}
