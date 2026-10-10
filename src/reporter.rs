use core::future::Future;
use core::marker::PhantomData;

use proxima_primitives::pipe::Pipe;

use crate::outcomes::TerminalOutcome;

/// Borrowed input for writing terminal results into caller-owned storage.
pub struct ReportInput<'outcomes, 'output> {
    /// Completed outcomes in the order they should be displayed.
    pub outcomes: &'outcomes [TerminalOutcome],
    /// Whether terminal labels should include ANSI color sequences.
    pub color: bool,
    /// Caller-owned destination; a short buffer remains untouched.
    pub output: &'output mut [u8],
}

/// Writes compact terminal outcome labels without allocating.
pub struct ReporterPipe<'outcomes, 'output>(
    PhantomData<(&'outcomes [TerminalOutcome], &'output mut [u8])>,
);

impl<'outcomes, 'output> ReporterPipe<'outcomes, 'output> {
    /// Creates a reporter for caller-owned outcome and output storage.
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<'outcomes, 'output> Pipe for ReporterPipe<'outcomes, 'output> {
    type In = ReportInput<'outcomes, 'output>;
    type Out = usize;
    type Err = ReportError;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        async move {
            let required: usize = input
                .outcomes
                .iter()
                .map(|outcome| label(*outcome, input.color).len())
                .sum();
            if required > input.output.len() {
                return Err(ReportError::OutputTooSmall);
            }
            let mut written = 0;
            for outcome in input.outcomes {
                let bytes = label(*outcome, input.color);
                input.output[written..written + bytes.len()].copy_from_slice(bytes);
                written += bytes.len();
            }
            Ok(written)
        }
    }
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
        (TerminalOutcome::Passed, false) => b"PASS\n",
        (TerminalOutcome::Failed, false) => b"FAIL\n",
    }
}

#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    use proxima_primitives::pipe::Pipe;

    use super::{ReportError, ReportInput, ReporterPipe};
    use crate::outcomes::TerminalOutcome;

    fn run<'outcomes, 'output>(
        input: ReportInput<'outcomes, 'output>,
    ) -> Result<usize, ReportError> {
        let pipe = ReporterPipe::new();
        let future = pipe.call(input);
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match Future::poll(future.as_mut(), &mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("reporter pipe unexpectedly yielded"),
        }
    }

    #[test]
    fn writes_colored_and_plain_records_and_rejects_short_buffer_atomically() {
        let outcomes = [TerminalOutcome::Passed, TerminalOutcome::Failed];
        let (formatted, allocations) = crate::dispatch_stream::tests::measure_allocations(|| {
            let mut colored = [0; 32];
            let colored_length = run(ReportInput {
                outcomes: &outcomes,
                color: true,
                output: &mut colored,
            })
            .expect("colored records fit");
            let mut plain = [0; 10];
            let plain_length = run(ReportInput {
                outcomes: &outcomes,
                color: false,
                output: &mut plain,
            })
            .expect("plain records fit");
            let mut short = [0x7f; 9];
            let short_result = run(ReportInput {
                outcomes: &outcomes,
                color: false,
                output: &mut short,
            });
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
