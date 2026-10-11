use std::fmt::Write;

use crate::burn::Calibration;
use crate::config::Config;
use crate::emit::{binary_name, crate_name, test_name};
use crate::plan::{Plan, PlannedTest};

const SLOWEST_COUNT: usize = 20;

fn json_string(text: &str) -> String {
    let mut escaped = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            control if u32::from(control) < 0x20 => {
                let _ = write!(escaped, "\\u{:04x}", u32::from(control));
            }
            other => escaped.push(other),
        }
    }
    escaped.push('"');
    escaped
}

fn test_json(test: &PlannedTest) -> String {
    format!(
        "{{\"crate\": {}, \"binary\": {}, \"test\": {}, \"planned_ms\": {:.3}, \"iterations\": {}, \"slow\": {}}}",
        json_string(&crate_name(test.crate_index)),
        json_string(&binary_name(test.binary_index)),
        json_string(&test_name(test.test_index)),
        test.planned_ms,
        test.iterations,
        test.slow
    )
}

fn slowest(plan: &Plan) -> Vec<&PlannedTest> {
    let mut ranked: Vec<&PlannedTest> = plan.tests.iter().collect();
    ranked.sort_by(|left, right| right.planned_ms.total_cmp(&left.planned_ms));
    ranked.truncate(SLOWEST_COUNT);
    ranked
}

fn list(items: &[String]) -> String {
    format!("[\n    {}\n  ]", items.join(",\n    "))
}

#[must_use]
pub fn render(config: &Config, calibration: &Calibration, plan: &Plan) -> String {
    let all: Vec<String> = plan.tests.iter().map(test_json).collect();
    let top: Vec<String> = slowest(plan).into_iter().map(test_json).collect();
    let target = config.target_serial_secs.map_or_else(|| "null".to_string(), |secs| format!("{secs:.3}"));
    format!(
        "{{\n  \"seed\": {},\n  \"counts\": {{\"crates\": {}, \"binaries_per_crate\": {}, \"tests_per_binary\": {}, \"tests\": {}}},\n  \
         \"distribution\": {{\"requested_median_ms\": {:.3}, \"effective_median_ms\": {:.6}, \"tail_sigma\": {:.3}, \"slow_fraction\": {:.4}, \"slow_ms\": {:.3}, \"target_serial_secs\": {target}}},\n  \
         \"calibration\": {{\"iters_per_ms\": {:.3}, \"rounds\": {}, \"origin\": {}, \"generator_profile\": {}}},\n  \
         \"total_planned_serial_ms\": {:.3},\n  \"slowest\": {},\n  \"tests\": {}\n}}\n",
        config.seed,
        config.crates,
        config.binaries_per_crate,
        config.tests_per_binary,
        plan.tests.len(),
        config.median_ms,
        plan.effective_median_ms,
        config.tail_sigma,
        config.slow_fraction,
        config.slow_ms,
        calibration.iters_per_ms,
        calibration.rounds,
        json_string(calibration.origin),
        json_string(calibration.generator_profile),
        plan.total_ms,
        list(&top),
        list(&all),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_quotes_backslashes_and_controls() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }
}
