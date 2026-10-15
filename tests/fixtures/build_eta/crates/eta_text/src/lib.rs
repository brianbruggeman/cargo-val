use eta_core::split_verb;

pub fn path_of(line: &str) -> Option<&str> {
    split_verb(line).filter(|(verb, _)| *verb == "GET").map(|(_, path)| path.trim_start_matches('/'))
}
