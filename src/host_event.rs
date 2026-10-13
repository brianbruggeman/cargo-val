use crate::schedule::{SessionSlotId, SessionSlots};

/// One child readiness event associated with a generation-checked slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEvent<'bytes> {
    /// Bytes read from a child's standard output buffer.
    Stdout {
        slot: SessionSlotId,
        bytes: &'bytes [u8],
    },
    /// Bytes read from a child's standard error buffer.
    Stderr {
        slot: SessionSlotId,
        bytes: &'bytes [u8],
    },
    /// Child standard output reached EOF.
    StdoutEof { slot: SessionSlotId },
    /// Child standard error reached EOF.
    StderrEof { slot: SessionSlotId },
    /// One terminal status for the child occupying this slot.
    Exit { slot: SessionSlotId, status: i32 },
}

/// Rejected host event because its slot is no longer live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleHostEvent {
    /// The retired or unknown session slot from the event.
    pub slot: SessionSlotId,
}

impl HostEvent<'_> {
    /// The generation-checked slot this event addresses.
    #[must_use]
    pub const fn slot(&self) -> SessionSlotId {
        match *self {
            Self::Stdout { slot, .. }
            | Self::Stderr { slot, .. }
            | Self::StdoutEof { slot }
            | Self::StderrEof { slot }
            | Self::Exit { slot, .. } => slot,
        }
    }
}

impl HostEvent<'_> {
    /// Returns the event unchanged when its slot generation is still live; a slot
    /// stays live through stream drain after `Exit`.
    ///
    /// # Errors
    ///
    /// Returns [`StaleHostEvent`] when the slot generation was retired.
    pub fn admit<T, const CAPACITY: usize>(
        self,
        sessions: &SessionSlots<T, CAPACITY>,
    ) -> Result<Self, StaleHostEvent> {
        let slot = self.slot();
        match sessions.get(slot) {
            Some(_) => Ok(self),
            None => Err(StaleHostEvent { slot }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HostEvent;
    use crate::outcomes::{OutcomeLedger, TerminalOutcome};
    use crate::schedule::SessionSlots;

    fn deliver<'bytes, T, const CAPACITY: usize>(
        sessions: &SessionSlots<T, CAPACITY>,
        event: HostEvent<'bytes>,
    ) -> Result<HostEvent<'bytes>, super::StaleHostEvent> {
        event.admit(sessions)
    }

    #[proxima::test]
    async fn c48h_validates_a_borrowed_event_through_the_ledger_view() {
        let (observation, allocations) = crate::test_alloc::measure_allocations(|| {
            let mut sessions = SessionSlots::<u8, 1>::new();
            let mut ledger = OutcomeLedger::new(&mut sessions).expect("table starts empty");
            let slot = ledger.start(9).expect("session slot is available");
            let output_bytes = b"out";
            let accepted = deliver(
                ledger.sessions(),
                HostEvent::Stdout {
                    slot,
                    bytes: output_bytes,
                },
            );

            let borrowed_pointer_identity = match accepted {
                Ok(HostEvent::Stdout { bytes, .. }) => bytes.as_ptr() == output_bytes.as_ptr(),
                Ok(
                    HostEvent::Stderr { .. }
                    | HostEvent::StdoutEof { .. }
                    | HostEvent::StderrEof { .. }
                    | HostEvent::Exit { .. },
                )
                | Err(_) => false,
            };
            let live_before_retirement = ledger.sessions().len();
            let retired_child = ledger.finish(slot, TerminalOutcome::Passed);
            (
                borrowed_pointer_identity,
                live_before_retirement,
                retired_child,
            )
        });
        let (borrowed_pointer_identity, live_before_retirement, retired_child) = observation;

        assert!(borrowed_pointer_identity);
        assert_eq!(live_before_retirement, 1);
        assert_eq!(retired_child, Ok(9));
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"host_event_validated_through_ledger\":1,\"borrowed_pointer_identity\":1,\"live_before_retirement\":1,\"retired_after_outcome\":1,\"allocations\":{allocations}}}"
        );
    }

    #[proxima::test]
    async fn c48g_events_borrow_output_and_reject_exit_for_retired_generation() {
        let (result, allocations) = crate::test_alloc::measure_allocations(|| {
            let mut sessions = SessionSlots::<(), 1>::new();
            let retired = sessions
                .insert(())
                .expect("first session slot is available");
            sessions.remove(retired).expect("first session is retired");
            let active = sessions.insert(()).expect("retired slot is reused");
            let stdout = b"out";
            let stderr = b"err";
            let output = deliver(
                &sessions,
                HostEvent::Stdout {
                    slot: active,
                    bytes: stdout,
                },
            );
            let error = deliver(
                &sessions,
                HostEvent::Stderr {
                    slot: active,
                    bytes: stderr,
                },
            );
            let exit = deliver(
                &sessions,
                HostEvent::Exit {
                    slot: active,
                    status: 0,
                },
            );
            let stale_exit = deliver(
                &sessions,
                HostEvent::Exit {
                    slot: retired,
                    status: 1,
                },
            );
            (output, error, exit, stale_exit, stdout, stderr)
        });
        let (output, error, exit, stale_exit, stdout, stderr) = result;

        match output.expect("stdout event belongs to live slot") {
            HostEvent::Stdout { bytes, .. } => assert_eq!(bytes.as_ptr(), stdout.as_ptr()),
            HostEvent::Stderr { .. }
            | HostEvent::StdoutEof { .. }
            | HostEvent::StderrEof { .. }
            | HostEvent::Exit { .. } => {
                panic!("stdout event kind changed")
            }
        }
        match error.expect("stderr event belongs to live slot") {
            HostEvent::Stderr { bytes, .. } => assert_eq!(bytes.as_ptr(), stderr.as_ptr()),
            HostEvent::Stdout { .. }
            | HostEvent::StdoutEof { .. }
            | HostEvent::StderrEof { .. }
            | HostEvent::Exit { .. } => {
                panic!("stderr event kind changed")
            }
        }
        assert!(matches!(exit, Ok(HostEvent::Exit { status: 0, .. })));
        assert!(stale_exit.is_err());
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"stdout\":1,\"stderr\":1,\"exit\":1,\"retired_generation_rejected\":1,\"borrowed_payloads\":2,\"payload_copies\":0,\"allocations\":0}}"
        );
    }

    #[proxima::test]
    async fn c48g_exit_keeps_slot_live_for_late_stream_output() {
        let (result, allocations) = crate::test_alloc::measure_allocations(|| {
            let mut sessions = SessionSlots::<u8, 1>::new();
            let slot = sessions.insert(7).expect("session slot is available");
            let exit = deliver(&sessions, HostEvent::Exit { slot, status: 0 });
            let stdout_bytes = b"late stdout";
            let stdout = deliver(
                &sessions,
                HostEvent::Stdout {
                    slot,
                    bytes: stdout_bytes,
                },
            );
            let stderr_bytes = b"late stderr";
            let stderr = deliver(
                &sessions,
                HostEvent::Stderr {
                    slot,
                    bytes: stderr_bytes,
                },
            );
            let live_sessions = sessions.len();
            let mut control_sessions = SessionSlots::<u8, 1>::new();
            let control_slot = control_sessions
                .insert(9)
                .expect("negative-control slot is available");
            let control_exit = deliver(
                &control_sessions,
                HostEvent::Exit {
                    slot: control_slot,
                    status: 0,
                },
            );
            control_sessions
                .remove(control_slot)
                .expect("negative control retires slot on exit");
            let control_stdout = deliver(
                &control_sessions,
                HostEvent::Stdout {
                    slot: control_slot,
                    bytes: stdout_bytes,
                },
            );
            (
                exit,
                stdout,
                stdout_bytes,
                stderr,
                stderr_bytes,
                live_sessions,
                control_exit,
                control_stdout,
            )
        });
        let (
            exit,
            stdout,
            stdout_bytes,
            stderr,
            stderr_bytes,
            live_sessions,
            control_exit,
            control_stdout,
        ) = result;

        assert!(matches!(exit, Ok(HostEvent::Exit { status: 0, .. })));
        assert_eq!(live_sessions, 1);
        match stdout.expect("late output belongs to the live generation") {
            HostEvent::Stdout { bytes: actual, .. } => {
                assert_eq!(actual.as_ptr(), stdout_bytes.as_ptr());
            }
            HostEvent::Stderr { .. }
            | HostEvent::StdoutEof { .. }
            | HostEvent::StderrEof { .. }
            | HostEvent::Exit { .. } => {
                panic!("stdout event kind changed")
            }
        }
        match stderr.expect("late stderr belongs to the live generation") {
            HostEvent::Stderr { bytes: actual, .. } => {
                assert_eq!(actual.as_ptr(), stderr_bytes.as_ptr());
            }
            HostEvent::Stdout { .. }
            | HostEvent::StdoutEof { .. }
            | HostEvent::StderrEof { .. }
            | HostEvent::Exit { .. } => {
                panic!("stderr event kind changed")
            }
        }
        assert!(matches!(
            control_exit,
            Ok(HostEvent::Exit { status: 0, .. })
        ));
        assert!(control_stdout.is_err());
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"exit_accepted\":1,\"late_stdout_accepted\":1,\"late_stderr_accepted\":1,\"borrowed_pointer_equalities\":2,\"live_sessions\":{live_sessions},\"removed_slot_control_rejected\":1,\"payload_copies\":0,\"allocations\":{allocations}}}"
        );
    }

    #[proxima::test]
    async fn c48j_accepts_eof_for_each_live_child_stream() {
        let (events, allocations) = crate::test_alloc::measure_allocations(|| {
            let mut sessions = SessionSlots::<(), 1>::new();
            let slot = sessions.insert(()).expect("session slot is available");
            let stdout_eof = deliver(&sessions, HostEvent::StdoutEof { slot });
            let stderr_eof = deliver(&sessions, HostEvent::StderrEof { slot });
            (stdout_eof, stderr_eof, sessions.len())
        });
        let (stdout_eof, stderr_eof, live_sessions) = events;

        assert!(matches!(stdout_eof, Ok(HostEvent::StdoutEof { .. })));
        assert!(matches!(stderr_eof, Ok(HostEvent::StderrEof { .. })));
        assert_eq!(live_sessions, 1);
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"stdout_eof_accepted\":1,\"stderr_eof_accepted\":1,\"live_sessions\":{live_sessions},\"payload_copies\":0,\"allocations\":{allocations}}}"
        );
    }

    #[proxima::test]
    async fn c48j_rejects_eof_from_a_retired_slot_generation() {
        let (events, allocations) = crate::test_alloc::measure_allocations(|| {
            let mut sessions = SessionSlots::<(), 1>::new();
            let retired = sessions
                .insert(())
                .expect("first session slot is available");
            sessions.remove(retired).expect("first session is retired");
            let active = sessions.insert(()).expect("retired slot is reused");
            let stale_stdout_eof = deliver(&sessions, HostEvent::StdoutEof { slot: retired });
            let stale_stderr_eof = deliver(&sessions, HostEvent::StderrEof { slot: retired });
            (
                stale_stdout_eof,
                stale_stderr_eof,
                sessions.get(active).is_some(),
            )
        });
        let (stale_stdout_eof, stale_stderr_eof, active_session_retained) = events;

        assert!(stale_stdout_eof.is_err());
        assert!(stale_stderr_eof.is_err());
        assert!(active_session_retained);
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"stale_stdout_eof_rejected\":1,\"stale_stderr_eof_rejected\":1,\"active_session_retained\":1,\"payload_copies\":0,\"allocations\":{allocations}}}"
        );
    }
}
