use cargo_val::render::Outcome;

const FIELD_GAP: &str = "  ";
const SOLO_MARK: &str = "  (solo rerun)";

/// Every settled test in a `--output plain` stdout, as `(binary, test, outcome)` in print order.
///
/// The grammar is `src/render.rs` `event_line`: a label, the binary, the test (each after one space), then an optional `  <seconds>s` and `  (solo rerun)`. Test names may contain single spaces (doctests do).
#[must_use]
pub fn settled(stdout: &str) -> Vec<(String, String, Outcome)> {
    stdout.lines().filter_map(settled_line).collect()
}

/// The test names that settled as `outcome`, in print order.
#[must_use]
pub fn names(stdout: &str, outcome: Outcome) -> Vec<String> {
    settled(stdout).into_iter().filter(|(_, _, settled)| *settled == outcome).map(|(_, test, _)| test).collect()
}

fn settled_line(line: &str) -> Option<(String, String, Outcome)> {
    let (label, line) = line.split_once(' ')?;
    let outcome = match label {
        "PASS" => Outcome::Passed,
        "FAIL" => Outcome::Failed,
        "IGNORED" => Outcome::Ignored,
        _ => return None,
    };
    let identity = line.trim_end().trim_end_matches(SOLO_MARK);
    let (binary, rest) = identity.split_once(' ')?;
    Some((binary.to_owned(), without_seconds(rest).to_owned(), outcome))
}

fn without_seconds(rest: &str) -> &str {
    match rest.rsplit_once(FIELD_GAP) {
        Some((test, seconds)) if is_seconds(seconds) => test,
        _ => rest,
    }
}

fn is_seconds(field: &str) -> bool {
    field.strip_suffix('s').and_then(|digits| digits.split_once('.')).is_some_and(|(whole, fraction)| {
        !whole.is_empty() && !fraction.is_empty() && whole.bytes().chain(fraction.bytes()).all(|byte| byte.is_ascii_digit())
    })
}
