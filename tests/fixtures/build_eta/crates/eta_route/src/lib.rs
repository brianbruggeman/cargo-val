use eta_text::path_of;

pub fn route(line: &str) -> String {
    path_of(line).map_or_else(|| "rejected".to_string(), |path| path.replace('/', ":"))
}
