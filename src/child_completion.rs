//! Sans-IO child lifecycle join retained in the coordinator-owned session slot.

/// One lifecycle event from the child and its captured streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildCompletionEvent {
    /// The operating system reported the child's terminal status.
    Exit,
    /// Unix stdout produced borrowed bytes while the stream remains open.
    StdoutBytes,
    /// Unix stderr produced borrowed bytes while the stream remains open.
    StderrBytes,
    /// The child's standard output pipe reached EOF.
    StdoutEof,
    /// A Windows stdout read completion reported EOF.
    WindowsStdoutReadEof,
    /// The child's standard error pipe reached EOF.
    StderrEof,
    /// A Windows stderr read completion reported EOF.
    WindowsStderrReadEof,
    /// The Windows stdout overlapped read is using its fixed buffer.
    WindowsStdoutReadPending,
    /// Windows completed the stdout read and exposed bytes for consumption.
    WindowsStdoutReadCompleted,
    /// Windows completed a stdout read with zero bytes and kept the stream open.
    WindowsStdoutReadEmpty,
    /// The caller consumed stdout bytes and released the fixed buffer.
    WindowsStdoutReadReleased,
    /// Windows confirmed stdout read cancellation and released its buffer.
    WindowsStdoutReadCancelled,
    /// The Windows stderr overlapped read is using its fixed buffer.
    WindowsStderrReadPending,
    /// Windows completed the stderr read and exposed bytes for consumption.
    WindowsStderrReadCompleted,
    /// Windows completed a stderr read with zero bytes and kept the stream open.
    WindowsStderrReadEmpty,
    /// The caller consumed stderr bytes and released the fixed buffer.
    WindowsStderrReadReleased,
    /// Windows confirmed stderr read cancellation and released its buffer.
    WindowsStderrReadCancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadState {
    Available,
    Pending,
    Ready,
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionState {
    Running,
    Exit,
    Stdout,
    Stderr,
    ExitAndStdout,
    ExitAndStderr,
    BothStreams,
    Complete,
}

/// Fixed-size completion state owned by one live child session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildCompletion {
    completion: CompletionState,
    stdout_read: ReadState,
    stderr_read: ReadState,
}

/// The event is invalid for the current child lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidChildCompletionEvent;

impl ChildCompletion {
    /// Creates a running child with both output streams open.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            completion: CompletionState::Running,
            stdout_read: ReadState::Available,
            stderr_read: ReadState::Available,
        }
    }

    /// Applies one child event and reports whether the slot can be retired.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidChildCompletionEvent`] when the event is not legal in the current read, wait, or exit state.
    pub fn apply(
        &mut self,
        event: ChildCompletionEvent,
    ) -> Result<bool, InvalidChildCompletionEvent> {
        match event {
            ChildCompletionEvent::Exit => {
                self.completion = match self.completion {
                    CompletionState::Running => CompletionState::Exit,
                    CompletionState::Stdout => CompletionState::ExitAndStdout,
                    CompletionState::Stderr => CompletionState::ExitAndStderr,
                    CompletionState::BothStreams => CompletionState::Complete,
                    CompletionState::Exit
                    | CompletionState::ExitAndStdout
                    | CompletionState::ExitAndStderr
                    | CompletionState::Complete => return Err(InvalidChildCompletionEvent),
                };
            }
            ChildCompletionEvent::StdoutBytes
            | ChildCompletionEvent::StdoutEof
            | ChildCompletionEvent::WindowsStdoutReadEof
            | ChildCompletionEvent::WindowsStdoutReadPending
            | ChildCompletionEvent::WindowsStdoutReadCancelled
            | ChildCompletionEvent::WindowsStdoutReadEmpty
            | ChildCompletionEvent::WindowsStdoutReadCompleted
            | ChildCompletionEvent::WindowsStdoutReadReleased => self.apply_stdout(event)?,
            ChildCompletionEvent::StderrBytes
            | ChildCompletionEvent::StderrEof
            | ChildCompletionEvent::WindowsStderrReadEof
            | ChildCompletionEvent::WindowsStderrReadPending
            | ChildCompletionEvent::WindowsStderrReadCancelled
            | ChildCompletionEvent::WindowsStderrReadEmpty
            | ChildCompletionEvent::WindowsStderrReadCompleted
            | ChildCompletionEvent::WindowsStderrReadReleased => self.apply_stderr(event)?,
        }
        Ok(matches!(self.completion, CompletionState::Complete)
            && !matches!(self.stdout_read, ReadState::Pending | ReadState::Ready)
            && !matches!(self.stderr_read, ReadState::Pending | ReadState::Ready))
    }

    fn apply_stdout(
        &mut self,
        event: ChildCompletionEvent,
    ) -> Result<(), InvalidChildCompletionEvent> {
        match event {
            ChildCompletionEvent::StdoutBytes => {
                if !matches!(self.stdout_read, ReadState::Available) {
                    return Err(InvalidChildCompletionEvent);
                }
            }
            ChildCompletionEvent::StdoutEof => {
                if matches!(self.stdout_read, ReadState::Pending | ReadState::Ready) {
                    return Err(InvalidChildCompletionEvent);
                }
                self.mark_stdout_eof()?;
                self.stdout_read = ReadState::Eof;
            }
            ChildCompletionEvent::WindowsStdoutReadEof => {
                if !matches!(self.stdout_read, ReadState::Available | ReadState::Pending) {
                    return Err(InvalidChildCompletionEvent);
                }
                self.mark_stdout_eof()?;
                self.stdout_read = ReadState::Eof;
            }
            ChildCompletionEvent::WindowsStdoutReadPending => {
                if matches!(
                    self.completion,
                    CompletionState::Stdout
                        | CompletionState::ExitAndStdout
                        | CompletionState::BothStreams
                        | CompletionState::Complete
                ) || !matches!(self.stdout_read, ReadState::Available)
                {
                    return Err(InvalidChildCompletionEvent);
                }
                self.stdout_read = ReadState::Pending;
            }
            ChildCompletionEvent::WindowsStdoutReadCancelled
            | ChildCompletionEvent::WindowsStdoutReadEmpty => {
                if !matches!(self.stdout_read, ReadState::Pending) {
                    return Err(InvalidChildCompletionEvent);
                }
                self.stdout_read = ReadState::Available;
            }
            ChildCompletionEvent::WindowsStdoutReadCompleted => {
                if !matches!(self.stdout_read, ReadState::Pending) {
                    return Err(InvalidChildCompletionEvent);
                }
                self.stdout_read = ReadState::Ready;
            }
            ChildCompletionEvent::WindowsStdoutReadReleased => {
                if !matches!(self.stdout_read, ReadState::Ready) {
                    return Err(InvalidChildCompletionEvent);
                }
                self.stdout_read = ReadState::Available;
            }
            _ => return Err(InvalidChildCompletionEvent),
        }
        Ok(())
    }

    fn apply_stderr(
        &mut self,
        event: ChildCompletionEvent,
    ) -> Result<(), InvalidChildCompletionEvent> {
        match event {
            ChildCompletionEvent::StderrBytes => {
                if !matches!(self.stderr_read, ReadState::Available) {
                    return Err(InvalidChildCompletionEvent);
                }
            }
            ChildCompletionEvent::StderrEof => {
                if matches!(self.stderr_read, ReadState::Pending | ReadState::Ready) {
                    return Err(InvalidChildCompletionEvent);
                }
                self.mark_stderr_eof()?;
                self.stderr_read = ReadState::Eof;
            }
            ChildCompletionEvent::WindowsStderrReadEof => {
                if !matches!(self.stderr_read, ReadState::Available | ReadState::Pending) {
                    return Err(InvalidChildCompletionEvent);
                }
                self.mark_stderr_eof()?;
                self.stderr_read = ReadState::Eof;
            }
            ChildCompletionEvent::WindowsStderrReadPending => {
                if matches!(
                    self.completion,
                    CompletionState::Stderr
                        | CompletionState::ExitAndStderr
                        | CompletionState::BothStreams
                        | CompletionState::Complete
                ) || !matches!(self.stderr_read, ReadState::Available)
                {
                    return Err(InvalidChildCompletionEvent);
                }
                self.stderr_read = ReadState::Pending;
            }
            ChildCompletionEvent::WindowsStderrReadCancelled
            | ChildCompletionEvent::WindowsStderrReadEmpty => {
                if !matches!(self.stderr_read, ReadState::Pending) {
                    return Err(InvalidChildCompletionEvent);
                }
                self.stderr_read = ReadState::Available;
            }
            ChildCompletionEvent::WindowsStderrReadCompleted => {
                if !matches!(self.stderr_read, ReadState::Pending) {
                    return Err(InvalidChildCompletionEvent);
                }
                self.stderr_read = ReadState::Ready;
            }
            ChildCompletionEvent::WindowsStderrReadReleased => {
                if !matches!(self.stderr_read, ReadState::Ready) {
                    return Err(InvalidChildCompletionEvent);
                }
                self.stderr_read = ReadState::Available;
            }
            _ => return Err(InvalidChildCompletionEvent),
        }
        Ok(())
    }

    fn mark_stdout_eof(&mut self) -> Result<(), InvalidChildCompletionEvent> {
        self.completion = match self.completion {
            CompletionState::Running => CompletionState::Stdout,
            CompletionState::Exit => CompletionState::ExitAndStdout,
            CompletionState::Stderr => CompletionState::BothStreams,
            CompletionState::ExitAndStderr => CompletionState::Complete,
            CompletionState::Stdout
            | CompletionState::ExitAndStdout
            | CompletionState::BothStreams
            | CompletionState::Complete => return Err(InvalidChildCompletionEvent),
        };
        Ok(())
    }

    fn mark_stderr_eof(&mut self) -> Result<(), InvalidChildCompletionEvent> {
        self.completion = match self.completion {
            CompletionState::Running => CompletionState::Stderr,
            CompletionState::Exit => CompletionState::ExitAndStderr,
            CompletionState::Stdout => CompletionState::BothStreams,
            CompletionState::ExitAndStdout => CompletionState::Complete,
            CompletionState::Stderr
            | CompletionState::ExitAndStderr
            | CompletionState::BothStreams
            | CompletionState::Complete => return Err(InvalidChildCompletionEvent),
        };
        Ok(())
    }
}

impl Default for ChildCompletion {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{ChildCompletion, ChildCompletionEvent};

    #[test]
    fn c48i_retires_after_exit_and_streams_in_either_order() {
        let ((), allocations) = crate::test_alloc::measure_allocations(|| {
            let mut exit_first = ChildCompletion::new();
            assert!(
                !exit_first
                    .apply(ChildCompletionEvent::Exit)
                    .expect("first exit is accepted")
            );
            assert!(
                !exit_first
                    .apply(ChildCompletionEvent::StdoutEof)
                    .expect("stdout EOF is accepted")
            );
            assert!(
                exit_first
                    .apply(ChildCompletionEvent::StderrEof)
                    .expect("stderr EOF completes the child")
            );

            let mut eof_first = ChildCompletion::new();
            assert!(
                !eof_first
                    .apply(ChildCompletionEvent::StderrEof)
                    .expect("stderr EOF is accepted")
            );
            assert!(
                !eof_first
                    .apply(ChildCompletionEvent::StdoutEof)
                    .expect("stdout EOF is accepted")
            );
            assert!(
                eof_first
                    .apply(ChildCompletionEvent::Exit)
                    .expect("exit completes the child")
            );

            assert!(exit_first.apply(ChildCompletionEvent::Exit).is_err());
            assert!(
                exit_first
                    .apply(ChildCompletionEvent::WindowsStdoutReadPending)
                    .is_err()
            );

            let mut read_after_eof = ChildCompletion::new();
            assert!(
                !read_after_eof
                    .apply(ChildCompletionEvent::StdoutEof)
                    .expect("stdout EOF is accepted")
            );
            assert!(
                read_after_eof
                    .apply(ChildCompletionEvent::WindowsStdoutReadPending)
                    .is_err()
            );
            assert!(
                read_after_eof
                    .apply(ChildCompletionEvent::WindowsStdoutReadEof)
                    .is_err()
            );
        });
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"exit_first_retirements\":1,\"eof_first_retirements\":1,\"duplicate_retirements\":0,\"allocations\":{allocations}}}"
        );
    }

    fn windows_cancellation_scenario() {
        let mut completion = ChildCompletion::new();
        assert!(
            !completion
                .apply(ChildCompletionEvent::WindowsStdoutReadPending)
                .expect("stdout pending read is accepted")
        );
        assert!(
            !completion
                .apply(ChildCompletionEvent::WindowsStderrReadPending)
                .expect("stderr pending read is accepted")
        );
        assert!(
            !completion
                .apply(ChildCompletionEvent::Exit)
                .expect("exit is accepted")
        );
        assert!(
            !completion
                .apply(ChildCompletionEvent::WindowsStdoutReadCancelled)
                .expect("stdout cancellation releases its pending operation")
        );
        assert!(
            !completion
                .apply(ChildCompletionEvent::WindowsStderrReadCancelled)
                .expect("stderr cancellation releases its pending operation")
        );
        assert!(
            !completion
                .apply(ChildCompletionEvent::WindowsStdoutReadPending)
                .expect("stdout read can resume after its canceled operation")
        );
        assert!(
            !completion
                .apply(ChildCompletionEvent::WindowsStderrReadPending)
                .expect("stderr read can resume after its canceled operation")
        );
        assert!(
            !completion
                .apply(ChildCompletionEvent::WindowsStdoutReadEof)
                .expect("stdout EOF arrives while stderr remains open")
        );
        assert!(
            completion
                .apply(ChildCompletionEvent::WindowsStderrReadEof)
                .expect("stderr EOF completes the child")
        );
        assert!(
            completion
                .apply(ChildCompletionEvent::WindowsStderrReadCancelled)
                .is_err()
        );
    }

    fn windows_completed_read_scenario() {
        let mut completed_read = ChildCompletion::new();
        assert!(
            !completed_read
                .apply(ChildCompletionEvent::WindowsStdoutReadPending)
                .expect("stdout read starts")
        );
        assert!(
            !completed_read
                .apply(ChildCompletionEvent::WindowsStderrReadPending)
                .expect("stderr read starts concurrently")
        );
        assert!(
            !completed_read
                .apply(ChildCompletionEvent::Exit)
                .expect("exit is accepted while reads remain pending")
        );
        assert!(
            !completed_read
                .apply(ChildCompletionEvent::WindowsStdoutReadCompleted)
                .expect("stdout read completes with bytes")
        );
        assert!(
            !completed_read
                .apply(ChildCompletionEvent::WindowsStderrReadCompleted)
                .expect("stderr read completes with bytes")
        );
        assert!(
            completed_read
                .apply(ChildCompletionEvent::StdoutEof)
                .is_err()
        );
        assert!(
            !completed_read
                .apply(ChildCompletionEvent::WindowsStdoutReadReleased)
                .expect("stdout payload consumer releases borrowed storage")
        );
        assert!(
            !completed_read
                .apply(ChildCompletionEvent::WindowsStderrReadReleased)
                .expect("stderr payload consumer releases borrowed storage")
        );
        assert!(
            !completed_read
                .apply(ChildCompletionEvent::WindowsStdoutReadPending)
                .expect("stdout starts the read that will report EOF")
        );
        assert!(
            !completed_read
                .apply(ChildCompletionEvent::WindowsStderrReadPending)
                .expect("stderr starts the read that will report EOF")
        );
        assert!(
            !completed_read
                .apply(ChildCompletionEvent::WindowsStdoutReadEof)
                .expect("stdout read completes with EOF")
        );
        assert!(
            completed_read
                .apply(ChildCompletionEvent::WindowsStderrReadEof)
                .expect("stderr read completes with EOF")
        );
        assert!(
            completed_read
                .apply(ChildCompletionEvent::WindowsStdoutReadPending)
                .is_err()
        );
    }

    fn windows_empty_read_scenario() {
        let mut empty_read = ChildCompletion::new();
        assert!(
            !empty_read
                .apply(ChildCompletionEvent::WindowsStdoutReadPending)
                .expect("stdout read starts")
        );
        assert!(
            !empty_read
                .apply(ChildCompletionEvent::WindowsStdoutReadEmpty)
                .expect("empty read releases its operation without closing the stream")
        );
        assert!(
            !empty_read
                .apply(ChildCompletionEvent::WindowsStdoutReadPending)
                .expect("stdout can rearm after an empty read")
        );
        let mut empty_stderr_read = ChildCompletion::new();
        assert!(
            !empty_stderr_read
                .apply(ChildCompletionEvent::WindowsStderrReadPending)
                .expect("stderr read starts")
        );
        assert!(
            !empty_stderr_read
                .apply(ChildCompletionEvent::WindowsStderrReadEmpty)
                .expect("empty stderr read releases its operation without closing the stream")
        );
        assert!(
            !empty_stderr_read
                .apply(ChildCompletionEvent::WindowsStderrReadPending)
                .expect("stderr can rearm after an empty read")
        );
    }

    fn windows_withheld_stderr_scenario() {
        let mut withheld_stderr = ChildCompletion::new();
        assert!(
            !withheld_stderr
                .apply(ChildCompletionEvent::Exit)
                .expect("exit is accepted")
        );
        assert!(
            !withheld_stderr
                .apply(ChildCompletionEvent::StdoutEof)
                .expect("stdout EOF is accepted")
        );
    }

    #[test]
    fn c48i_waits_for_windows_read_cancellation_and_both_streams() {
        let ((), allocations) = crate::test_alloc::measure_allocations(|| {
            windows_cancellation_scenario();
            windows_completed_read_scenario();
            windows_empty_read_scenario();
            windows_withheld_stderr_scenario();
        });
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"retirements_while_read_pending\":0,\"retirements_after_both_cancellations_and_eofs\":1,\"normal_read_completions\":2,\"buffer_releases_before_retirement\":2,\"empty_read_rearms\":2,\"read_after_eof_rejected\":1,\"eof_while_buffer_ready_rejected\":1,\"withheld_stderr_retirements\":0,\"allocations\":{allocations}}}"
        );
    }
}
