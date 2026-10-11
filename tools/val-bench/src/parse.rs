pub fn cargo_test_tests_run(stdout: &str) -> u64 {
    stdout.lines().filter_map(cargo_test_result_count).sum()
}

fn cargo_test_result_count(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("test result: ")?;
    let passed = count_before(rest, " passed")?;
    let failed = count_before(rest, " failed")?;
    Some(passed + failed)
}

fn count_before(text: &str, marker: &str) -> Option<u64> {
    let end = text.find(marker)?;
    let head = &text[..end];
    let start = head.rfind([' ', ';', '.']).map_or(0, |index| index + 1);
    head[start..].parse().ok()
}

pub fn nextest_tests_run(stderr: &str) -> u64 {
    stderr
        .lines()
        .filter(|line| line.trim_start().starts_with("Summary ["))
        .filter_map(|line| line.split("] ").nth(1))
        .filter_map(|tail| tail.split(" test").next())
        .filter_map(|count| count.trim().parse::<u64>().ok())
        .sum()
}

pub fn cargo_val_tests_run(stdout: &str) -> u64 {
    let verdicts = stdout
        .lines()
        .filter(|line| line.ends_with(" PASS") || line.ends_with(" FAIL"))
        .count();
    u64::try_from(verdicts).unwrap_or(u64::MAX)
}

pub fn strip_ansi(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            plain.push(character);
            continue;
        }
        if characters.next() == Some('[') {
            characters.by_ref().find(|following| ('@'..='~').contains(following));
        }
    }
    plain
}

pub fn cargo_test_result_line(line: &str) -> bool {
    let Some(rest) = line.trim_end().strip_prefix("test ") else {
        return false;
    };
    rest.rsplit_once(" ... ")
        .is_some_and(|(_, verdict)| matches!(verdict, "ok" | "FAILED") || verdict.starts_with("ignored"))
}

pub fn nextest_result_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    ["PASS [", "FAIL [", "SKIP ["].iter().any(|prefix| trimmed.starts_with(prefix))
}

pub fn cargo_val_result_line(line: &str) -> bool {
    let plain = strip_ansi(line);
    let trimmed = plain.trim_end();
    [" PASS", " FAIL", " IGNORED"].iter().any(|suffix| trimmed.ends_with(suffix))
}

pub fn never_result_line(_: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARGO_TEST_FOUR: &str = "\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 4 filtered out; finished in 0.00s\n\n\nrunning 4 tests\ntest order_seed_thirty_one ... ok\ntest order_seed_thirty_seven ... ok\ntest order_seed_forty_one ... ok\ntest order_seed_forty_three ... ok\n\ntest result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\n";

    #[test]
    fn cargo_test_sums_passed_across_every_test_target() {
        assert_eq!(cargo_test_tests_run(CARGO_TEST_FOUR), 4);
    }

    #[test]
    fn cargo_test_counts_failed_tests_as_run_and_ignored_as_not_run() {
        let output = "test result: FAILED. 3 passed; 2 failed; 5 ignored; 0 measured; 0 filtered out; finished in 0.10s\n";
        assert_eq!(cargo_test_tests_run(output), 5);
    }

    #[test]
    fn cargo_test_with_no_result_lines_is_zero() {
        assert_eq!(cargo_test_tests_run("error: could not compile\n"), 0);
    }

    #[test]
    fn cargo_val_counts_one_per_verdict_line() {
        let stdout = "order_seed_forty_one PASS\norder_seed_forty_three PASS\norder_seed_thirty_seven PASS\norder_seed_thirty_one PASS\n";
        assert_eq!(cargo_val_tests_run(stdout), 4);
    }

    #[test]
    fn cargo_val_counts_fail_lines_and_ignores_other_text() {
        let stdout = "building\nfixture::passes PASS\nbranch_seed_five FAIL\ndone\n";
        assert_eq!(cargo_val_tests_run(stdout), 2);
    }

    #[test]
    fn nextest_reads_count_from_summary_line() {
        let stderr = "    Starting 17 tests across 6 binaries\n        PASS [   0.082s] ( 1/17) cargo_val_fixture::arithmetic_fixture arithmetic_seed_four\n     Summary [   0.204s] 17 tests run: 17 passed, 0 skipped\n";
        assert_eq!(nextest_tests_run(stderr), 17);
    }

    #[test]
    fn nextest_reads_singular_summary_line() {
        let stderr = "     Summary [   0.010s] 1 test run: 1 passed, 16 skipped\n";
        assert_eq!(nextest_tests_run(stderr), 1);
    }

    #[test]
    fn nextest_without_summary_is_zero() {
        assert_eq!(nextest_tests_run("error: no tests to run\n"), 0);
    }

    #[test]
    fn cargo_test_matches_ok_failed_and_ignored_lines_from_proxima_race2() {
        assert!(cargo_test_result_line("test config::tests::affinities_fluent_sets_count_and_placement ... ok"));
        assert!(cargo_test_result_line("test windows_port_udp_loopback_payload ... FAILED"));
        assert!(cargo_test_result_line("test proxima-process/src/capabilities.rs - capabilities (line 14) ... ignored"));
    }

    #[test]
    fn cargo_test_rejects_summary_header_and_compile_lines() {
        assert!(!cargo_test_result_line("test result: FAILED. 7 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.78s"));
        assert!(!cargo_test_result_line("running 4 tests"));
        assert!(!cargo_test_result_line("   Compiling proxima-example-new-platform v0.1.0 (/Users/brianbruggeman/repos/slot-0/proxima-prime-windows-io/examples/new-platform)"));
    }

    #[test]
    fn nextest_matches_pass_and_fail_lines_from_proxima_race2() {
        assert!(nextest_result_line("        PASS [   0.010s] (   1/4327) prime config::tests::affinity_config_and_fluent_paths_agree"));
        assert!(nextest_result_line("        FAIL [   0.008s] ( 149/4327) prime::windows_port windows_port_udp_loopback_payload"));
        assert!(nextest_result_line("        SKIP [         ] prime some::skipped_test"));
    }

    #[test]
    fn nextest_rejects_start_summary_and_echoed_test_lines() {
        assert!(!nextest_result_line("    Starting 17 tests across 6 binaries"));
        assert!(!nextest_result_line("     Summary [   0.204s] 17 tests run: 17 passed, 0 skipped"));
        assert!(!nextest_result_line("    test windows_port_udp_loopback_payload ... FAILED"));
    }

    #[test]
    fn cargo_val_matches_plain_verdict_lines_from_c62() {
        assert!(cargo_val_result_line("order_seed_forty_one PASS"));
        assert!(cargo_val_result_line("branch_seed_five FAIL"));
        assert!(cargo_val_result_line("slow_one IGNORED"));
    }

    #[test]
    fn cargo_val_matches_ansi_verdict_lines_from_proxima_race2() {
        assert!(cargo_val_result_line("describe::tests::rejects_an_enum \u{1b}[32mPASS\u{1b}[0m"));
        assert!(cargo_val_result_line("describe::tests::rejects_a_tuple_struct \u{1b}[31mFAIL\u{1b}[0m"));
        assert!(cargo_val_result_line("slow_one \u{1b}[33mIGNORED\u{1b}[0m"));
    }

    #[test]
    fn cargo_val_rejects_build_noise() {
        assert!(!cargo_val_result_line("   Compiling proxima-example-new-platform v0.1.0 (/Users/brianbruggeman/repos/slot-0/proxima-prime-windows-io/examples/new-platform)"));
        assert!(!cargo_val_result_line("PASS the build"));
    }

    #[test]
    fn strip_ansi_removes_color_escapes_and_keeps_text() {
        assert_eq!(strip_ansi("name \u{1b}[32mPASS\u{1b}[0m"), "name PASS");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
