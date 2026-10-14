use std::collections::BTreeMap;

const LABEL_WIDTH: usize = 9;

/// The `SUMMARY` line's `key=value` counts (`passed`, `failed`, `ignored`, `not_run`); `None` when no SUMMARY line was printed. The grammar is `src/render.rs` `closing`; `wall=` is not an integer and is left out.
#[must_use]
pub fn summary(stdout: &str) -> Option<BTreeMap<String, usize>> {
    let line = stdout.lines().find(|line| line.starts_with("SUMMARY "))?;
    let pairs = line[LABEL_WIDTH.min(line.len())..]
        .split_whitespace()
        .filter_map(|field| field.split_once('='))
        .filter_map(|(key, value)| Some((key.to_owned(), value.parse().ok()?)))
        .collect();
    Some(pairs)
}
