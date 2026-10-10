use core::future::Future;
use core::marker::PhantomData;

use proxima_primitives::pipe::Pipe;

use crate::ExecutablePath;
use crate::artifact_stream::ArtifactFramer;
use crate::dispatch_stream::{DispatchError, DispatchInput, DispatchPipe};
use crate::selection::{SelectionInput, SelectionMode, SelectionPipe, SelectionStep};
use crate::session_slots::SessionSlots;
use crate::test_intent::{ExactTestIntent, ExactTestIntentInput, ExactTestIntentPipe};

/// One caller-owned coordinator step over the current listing chunk.
pub struct CoordinatorInput<'call, 'buffer, 'slots, 'executable, 'scratch, T, const CAPACITY: usize>
{
    /// Fixed storage for one listing frame.
    pub framer: &'call mut ArtifactFramer<'buffer>,
    /// Active child sessions owned directly by the coordinator.
    pub sessions: &'slots mut SessionSlots<T, CAPACITY>,
    /// Unconsumed bytes from the current libtest listing read.
    pub listing_chunk: &'call [u8],
    /// Executable path retained from the Cargo artifact stream.
    pub executable: ExecutablePath<'executable, 'scratch>,
    /// Test-name filter.
    pub filter: &'call str,
    /// Exact or substring selection policy.
    pub selection_mode: SelectionMode,
    /// Maximum bytes in a test-name line, excluding its newline.
    pub max_line_bytes: usize,
}

/// One result from a pull-driven coordinator Pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatorStep<'call, 'executable, 'scratch> {
    /// One selected name is ready for immediate host launch.
    Launch {
        /// Borrowed exact-test process intent.
        intent: ExactTestIntent<'executable, 'scratch, 'call>,
        /// Bytes consumed from the current listing chunk.
        consumed_bytes: usize,
    },
    /// All live-session slots are occupied; listing bytes were not consumed.
    Full,
    /// One test name did not match the configured filter.
    Filtered { consumed_bytes: usize },
    /// One non-test line was consumed.
    Ignored { consumed_bytes: usize },
    /// No complete listing line was available.
    NeedMore { consumed_bytes: usize },
}

/// Pulls a single selected test and returns its launch intent without a queue.
pub struct CoordinatorPipe<'call, 'buffer, 'slots, 'executable, 'scratch, T, const CAPACITY: usize>(
    PhantomData<(
        &'call mut ArtifactFramer<'buffer>,
        &'slots mut SessionSlots<T, CAPACITY>,
        &'executable str,
        &'scratch str,
    )>,
);

impl<'call, 'buffer, 'slots, 'executable, 'scratch, T, const CAPACITY: usize>
    CoordinatorPipe<'call, 'buffer, 'slots, 'executable, 'scratch, T, CAPACITY>
{
    /// Creates one pull-driven coordinator Pipe.
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T, const CAPACITY: usize> Default for CoordinatorPipe<'_, '_, '_, '_, '_, T, CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'call, 'buffer, 'slots, 'executable, 'scratch, T, const CAPACITY: usize> Pipe
    for CoordinatorPipe<'call, 'buffer, 'slots, 'executable, 'scratch, T, CAPACITY>
{
    type In = CoordinatorInput<'call, 'buffer, 'slots, 'executable, 'scratch, T, CAPACITY>;
    type Out = CoordinatorStep<'call, 'executable, 'scratch>;
    type Err = DispatchError;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        async move {
            let dispatch = DispatchPipe::<T, CAPACITY>::new()
                .call(DispatchInput {
                    framer: input.framer,
                    sessions: input.sessions,
                    chunk: input.listing_chunk,
                    max_line_bytes: input.max_line_bytes,
                })
                .await?;
            let selection = match SelectionPipe::new()
                .call(SelectionInput {
                    dispatch,
                    filter: input.filter,
                    mode: input.selection_mode,
                })
                .await
            {
                Ok(selection) => selection,
                Err(error) => match error {},
            };
            Ok(match selection {
                SelectionStep::Selected {
                    name,
                    consumed_bytes,
                } => {
                    let intent = match ExactTestIntentPipe::new()
                        .call(ExactTestIntentInput {
                            executable: input.executable,
                            test_name: name,
                        })
                        .await
                    {
                        Ok(intent) => intent,
                        Err(error) => match error {},
                    };
                    CoordinatorStep::Launch {
                        intent,
                        consumed_bytes,
                    }
                }
                SelectionStep::Full => CoordinatorStep::Full,
                SelectionStep::Filtered { consumed_bytes, .. } => {
                    CoordinatorStep::Filtered { consumed_bytes }
                }
                SelectionStep::Ignored { consumed_bytes } => {
                    CoordinatorStep::Ignored { consumed_bytes }
                }
                SelectionStep::NeedMore { consumed_bytes } => {
                    CoordinatorStep::NeedMore { consumed_bytes }
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use std::string::String;

    use proxima_primitives::pipe::Pipe;

    use super::{CoordinatorInput, CoordinatorPipe, CoordinatorStep};
    use crate::ExecutablePath;
    use crate::artifact_stream::ArtifactFramer;
    use crate::host_event::{HostEvent, HostEventInput, HostEventPipe};
    use crate::selection::SelectionMode;
    use crate::session_slots::SessionSlots;

    fn retire_child<const CAPACITY: usize>(
        sessions: &mut SessionSlots<(), CAPACITY>,
        slot: crate::session_slots::SessionSlotId,
    ) {
        let pipe = HostEventPipe::new();
        let future = pipe.call(HostEventInput {
            event: HostEvent::Exit { slot, status: 0 },
            sessions,
        });
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match Future::poll(future.as_mut(), &mut context) {
            Poll::Ready(Ok(_)) => {}
            Poll::Ready(Err(error)) => panic!("child exit event was rejected: {error:?}"),
            Poll::Pending => panic!("child exit event unexpectedly yielded"),
        }
    }

    fn launch_count_matches(observed: usize, expected: usize) -> bool {
        observed == expected
    }

    fn eof_gated_control_launch(intent_available: bool, listing_eof_seen: bool) -> bool {
        intent_available && listing_eof_seen
    }

    fn run<'call, 'buffer, 'slots, 'executable, 'scratch, T, const CAPACITY: usize>(
        framer: &'call mut ArtifactFramer<'buffer>,
        sessions: &'slots mut SessionSlots<T, CAPACITY>,
        listing_chunk: &'call [u8],
        executable: ExecutablePath<'executable, 'scratch>,
    ) -> Result<CoordinatorStep<'call, 'executable, 'scratch>, crate::dispatch_stream::DispatchError>
    {
        let pipe = CoordinatorPipe::<T, CAPACITY>::new();
        let future = pipe.call(CoordinatorInput {
            framer,
            sessions,
            listing_chunk,
            executable,
            filter: "",
            selection_mode: SelectionMode::Substring,
            max_line_bytes: 64,
        });
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match Future::poll(future.as_mut(), &mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("coordinator Pipe unexpectedly yielded"),
        }
    }

    #[test]
    fn launches_before_eof_and_only_pulls_again_after_slot_retirement() {
        let listing = b"alpha: test\nbeta: test\n";
        let executable = String::from("target/debug/deps/fixture");
        let executable_path = executable.as_str();
        let (result, allocations) = crate::dispatch_stream::tests::measure_allocations(|| {
            let mut storage = [0; 65];
            let mut framer = ArtifactFramer::new(&mut storage, 64).expect("listing storage fits");
            let mut sessions = SessionSlots::<(), 1>::new();
            let first = run(
                &mut framer,
                &mut sessions,
                &listing[..12],
                ExecutablePath::Borrowed(executable_path),
            );
            let (first_launch, first_launch_count) = match &first {
                Ok(CoordinatorStep::Launch {
                    intent,
                    consumed_bytes,
                }) => {
                    let observed = intent.test_name() == "alpha" && *consumed_bytes == 12;
                    (observed, usize::from(observed))
                }
                Ok(CoordinatorStep::Full)
                | Ok(CoordinatorStep::Filtered { .. })
                | Ok(CoordinatorStep::Ignored { .. })
                | Ok(CoordinatorStep::NeedMore { .. })
                | Err(_) => (false, 0),
            };
            let first_slot = sessions.insert(()).expect("child slot available");
            let blocked = run(
                &mut framer,
                &mut sessions,
                &listing[12..],
                ExecutablePath::Borrowed(executable_path),
            );
            let blocked_without_consuming = matches!(blocked, Ok(CoordinatorStep::Full));
            retire_child(&mut sessions, first_slot);
            let resumed = run(
                &mut framer,
                &mut sessions,
                &listing[12..],
                ExecutablePath::Borrowed(executable_path),
            );
            let (second_launch, second_launch_count) = match &resumed {
                Ok(CoordinatorStep::Launch {
                    intent,
                    consumed_bytes,
                }) => {
                    let observed = intent.test_name() == "beta" && *consumed_bytes == 11;
                    (observed, usize::from(observed))
                }
                Ok(CoordinatorStep::Full)
                | Ok(CoordinatorStep::Filtered { .. })
                | Ok(CoordinatorStep::Ignored { .. })
                | Ok(CoordinatorStep::NeedMore { .. })
                | Err(_) => (false, 0),
            };
            (
                first_launch,
                first_launch_count,
                blocked_without_consuming,
                second_launch,
                second_launch_count,
            )
        });
        let (
            first_launch,
            first_launch_count,
            blocked_without_consuming,
            second_launch,
            second_launch_count,
        ) = result;
        assert!(first_launch);
        assert!(blocked_without_consuming);
        assert!(second_launch);
        assert!(launch_count_matches(first_launch_count, 1));
        assert_eq!(second_launch_count, 1);
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"first_launch_before_eof\":{first_launch_count},\"listing_bytes_consumed_while_full\":0,\"launch_after_exit\":{second_launch_count},\"allocations\":{allocations}}}"
        );
    }

    #[test]
    fn rejects_delaying_first_launch_until_listing_eof() {
        let mut storage = [0; 65];
        let mut framer = ArtifactFramer::new(&mut storage, 64).expect("listing storage fits");
        let mut sessions = SessionSlots::<(), 1>::new();
        let actual = run(
            &mut framer,
            &mut sessions,
            b"alpha: test\n",
            ExecutablePath::Borrowed("target/debug/deps/fixture"),
        );
        let actual_intent_available = matches!(actual, Ok(CoordinatorStep::Launch { .. }));
        let listing_eof_seen = false;
        let delayed_control_launch_before_eof =
            eof_gated_control_launch(actual_intent_available, listing_eof_seen);
        let delayed_control_launch_after_eof =
            eof_gated_control_launch(actual_intent_available, true);

        assert!(launch_count_matches(
            usize::from(actual_intent_available),
            1
        ));
        assert!(!launch_count_matches(
            usize::from(delayed_control_launch_before_eof),
            1
        ));
        assert!(delayed_control_launch_after_eof);
        std::println!(
            "{{\"actual_intent_before_eof\":{},\"delayed_control_before_eof\":{},\"delayed_control_after_eof\":{},\"late_launch_control_rejected\":1}}",
            usize::from(actual_intent_available),
            usize::from(delayed_control_launch_before_eof),
            usize::from(delayed_control_launch_after_eof)
        );
    }
}
