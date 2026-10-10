use core::future::Future;
use core::marker::PhantomData;

use proxima_primitives::pipe::Pipe;

use crate::session_slots::{SessionSlotId, SessionSlots};

/// One readiness payload associated with a generation-checked child slot.
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
    /// One terminal status for the child occupying this slot.
    Exit { slot: SessionSlotId, status: i32 },
}

/// Event accepted from the host edge while retaining borrowed output bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptedHostEvent<'bytes> {
    /// Accepted stdout payload.
    Stdout {
        slot: SessionSlotId,
        bytes: &'bytes [u8],
    },
    /// Accepted stderr payload.
    Stderr {
        slot: SessionSlotId,
        bytes: &'bytes [u8],
    },
    /// Accepted terminal status after retiring its live slot.
    Exit { slot: SessionSlotId, status: i32 },
}

/// Rejected host event because its slot is no longer live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleHostEvent {
    /// The retired or unknown session slot from the event.
    pub slot: SessionSlotId,
}

/// Borrowed pipe input joining one event to its coordinator-owned slots.
pub struct HostEventInput<'event, 'bytes, const CAPACITY: usize> {
    /// Event and any host-owned payload bytes.
    pub event: HostEvent<'bytes>,
    /// Mutable live-session table owned by the async coordinator.
    pub sessions: &'event mut SessionSlots<(), CAPACITY>,
}

/// Validates event generations and retires only the matching terminal slot.
pub struct HostEventPipe<'event, 'bytes, const CAPACITY: usize>(
    PhantomData<(&'event mut SessionSlots<(), CAPACITY>, HostEvent<'bytes>)>,
);

impl<'event, 'bytes, const CAPACITY: usize> HostEventPipe<'event, 'bytes, CAPACITY> {
    /// Creates one host-event validation step.
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<'event, 'bytes, const CAPACITY: usize> Pipe for HostEventPipe<'event, 'bytes, CAPACITY> {
    type In = HostEventInput<'event, 'bytes, CAPACITY>;
    type Out = AcceptedHostEvent<'bytes>;
    type Err = StaleHostEvent;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        async move {
            match input.event {
                HostEvent::Stdout { slot, bytes } => {
                    if input.sessions.get(slot).is_none() {
                        return Err(StaleHostEvent { slot });
                    }
                    Ok(AcceptedHostEvent::Stdout { slot, bytes })
                }
                HostEvent::Stderr { slot, bytes } => {
                    if input.sessions.get(slot).is_none() {
                        return Err(StaleHostEvent { slot });
                    }
                    Ok(AcceptedHostEvent::Stderr { slot, bytes })
                }
                HostEvent::Exit { slot, status } => {
                    if input.sessions.remove(slot).is_none() {
                        return Err(StaleHostEvent { slot });
                    }
                    Ok(AcceptedHostEvent::Exit { slot, status })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    use proxima_primitives::pipe::Pipe;

    use super::{AcceptedHostEvent, HostEvent, HostEventInput, HostEventPipe};
    use crate::session_slots::SessionSlots;

    fn deliver<'event, 'bytes, const CAPACITY: usize>(
        sessions: &'event mut SessionSlots<(), CAPACITY>,
        event: HostEvent<'bytes>,
    ) -> Result<AcceptedHostEvent<'bytes>, super::StaleHostEvent> {
        let pipe = HostEventPipe::new();
        let future = pipe.call(HostEventInput { event, sessions });
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match Future::poll(future.as_mut(), &mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("host-event pipe unexpectedly yielded"),
        }
    }

    #[test]
    fn events_borrow_output_and_reject_exit_for_retired_generation() {
        let (result, allocations) = crate::dispatch_stream::tests::measure_allocations(|| {
            let mut sessions = SessionSlots::<(), 1>::new();
            let retired = sessions
                .insert(())
                .expect("first session slot is available");
            sessions.remove(retired).expect("first session is retired");
            let active = sessions.insert(()).expect("retired slot is reused");
            let stdout = b"out";
            let stderr = b"err";
            let output = deliver(
                &mut sessions,
                HostEvent::Stdout {
                    slot: active,
                    bytes: stdout,
                },
            );
            let error = deliver(
                &mut sessions,
                HostEvent::Stderr {
                    slot: active,
                    bytes: stderr,
                },
            );
            let exit = deliver(
                &mut sessions,
                HostEvent::Exit {
                    slot: active,
                    status: 0,
                },
            );
            let stale_exit = deliver(
                &mut sessions,
                HostEvent::Exit {
                    slot: retired,
                    status: 1,
                },
            );
            (output, error, exit, stale_exit, stdout, stderr)
        });
        let (output, error, exit, stale_exit, stdout, stderr) = result;

        match output.expect("stdout event belongs to live slot") {
            AcceptedHostEvent::Stdout { bytes, .. } => assert_eq!(bytes.as_ptr(), stdout.as_ptr()),
            AcceptedHostEvent::Stderr { .. } | AcceptedHostEvent::Exit { .. } => {
                panic!("stdout event kind changed")
            }
        }
        match error.expect("stderr event belongs to live slot") {
            AcceptedHostEvent::Stderr { bytes, .. } => assert_eq!(bytes.as_ptr(), stderr.as_ptr()),
            AcceptedHostEvent::Stdout { .. } | AcceptedHostEvent::Exit { .. } => {
                panic!("stderr event kind changed")
            }
        }
        assert!(matches!(
            exit,
            Ok(AcceptedHostEvent::Exit { status: 0, .. })
        ));
        assert!(stale_exit.is_err());
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"stdout\":1,\"stderr\":1,\"exit\":1,\"retired_generation_rejected\":1,\"borrowed_payloads\":2,\"payload_copies\":0,\"allocations\":0}}"
        );
    }
}
