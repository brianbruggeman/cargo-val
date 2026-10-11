use crate::outcomes::TerminalOutcome;

/// Writes compact terminal outcome labels into caller-owned storage without
/// allocating; a short buffer is left untouched.
///
/// # Errors
///
/// Returns [`ReportError::OutputTooSmall`] when `output` cannot hold every label.
pub fn write_report(
    outcomes: &[TerminalOutcome],
    color: bool,
    output: &mut [u8],
) -> Result<usize, ReportError> {
    let required: usize = outcomes
        .iter()
        .map(|outcome| label(*outcome, color).len())
        .sum();
    if required > output.len() {
        return Err(ReportError::OutputTooSmall);
    }
    let mut written = 0;
    for outcome in outcomes {
        let bytes = label(*outcome, color);
        output[written..written + bytes.len()].copy_from_slice(bytes);
        written += bytes.len();
    }
    Ok(written)
}

/// Failure to format all records into the supplied output buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportError {
    /// The output buffer cannot hold every complete record.
    OutputTooSmall,
}

fn label(outcome: TerminalOutcome, color: bool) -> &'static [u8] {
    match (outcome, color) {
        (TerminalOutcome::Passed, true) => b"\x1b[32mPASS\x1b[0m\n",
        (TerminalOutcome::Failed, true) => b"\x1b[31mFAIL\x1b[0m\n",
        (TerminalOutcome::Ignored, true) => b"\x1b[33mIGNORED\x1b[0m\n",
        (TerminalOutcome::Passed, false) => b"PASS\n",
        (TerminalOutcome::Failed, false) => b"FAIL\n",
        (TerminalOutcome::Ignored, false) => b"IGNORED\n",
    }
}

#[cfg(test)]
mod tests {
    use super::{ReportError, write_report};
    use crate::outcomes::TerminalOutcome;

    #[test]
    fn writes_colored_and_plain_records_and_rejects_short_buffer_atomically() {
        let outcomes = [TerminalOutcome::Passed, TerminalOutcome::Failed];
        let (formatted, allocations) = crate::test_alloc::measure_allocations(|| {
            let mut colored = [0; 32];
            let colored_length =
                write_report(&outcomes, true, &mut colored).expect("colored records fit");
            let mut plain = [0; 10];
            let plain_length =
                write_report(&outcomes, false, &mut plain).expect("plain records fit");
            let mut short = [0x7f; 9];
            let short_result = write_report(&outcomes, false, &mut short);
            (
                colored,
                colored_length,
                plain,
                plain_length,
                short,
                short_result,
            )
        });
        let (colored, colored_length, plain, plain_length, short, short_result) = formatted;

        assert!(
            colored[..colored_length]
                .windows(5)
                .any(|window| window == b"\x1b[32m")
        );
        assert!(
            colored[..colored_length]
                .windows(5)
                .any(|window| window == b"\x1b[31m")
        );
        assert_eq!(&plain[..plain_length], b"PASS\nFAIL\n");
        assert_eq!(short_result, Err(ReportError::OutputTooSmall));
        assert!(short.iter().all(|byte| *byte == 0x7f));
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"ansi_records_true\":2,\"ansi_records_false\":0,\"short_buffer_errors\":1,\"short_buffer_unchanged\":1,\"allocations\":0}}"
        );
    }
}
