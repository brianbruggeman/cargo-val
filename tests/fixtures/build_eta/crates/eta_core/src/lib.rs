pub fn split_verb(line: &str) -> Option<(&str, &str)> {
    line.split_once(' ')
}
