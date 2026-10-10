use core::marker::PhantomData;

use proxima_primitives::pipe::Pipe;

use crate::artifact_stream::ArtifactFramer;
use crate::discovery_stream::{ListingDiscoveryError, ListingDiscoveryInput, ListingDiscoveryPipe};
use crate::libtest_listing::ListingLine;
use crate::session_slots::SessionSlots;

/// One pull from listing output, gated by the coordinator's live child slots.
pub struct DispatchInput<'call, 'buffer, 'slots, T, const CAPACITY: usize> {
    /// Caller-owned listing frame storage.
    pub framer: &'call mut ArtifactFramer<'buffer>,
    /// Live sessions owned by the single async coordinator.
    pub sessions: &'slots mut SessionSlots<T, CAPACITY>,
    /// Listing bytes; replay the unconsumed suffix before reading more.
    pub chunk: &'call [u8],
    /// Maximum bytes in a test-name line, excluding its newline.
    pub max_line_bytes: usize,
}

/// Result of one capacity-gated listing pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchStep<'line> {
    /// All slots contain live children; no listing byte was consumed.
    Full,
    /// One test is ready to launch before listing EOF.
    Test {
        name: &'line str,
        consumed_bytes: usize,
    },
    /// A complete non-test line was consumed.
    Ignored { consumed_bytes: usize },
    /// No complete line was available yet.
    NeedMore { consumed_bytes: usize },
}

/// Failure while framing or parsing a listing record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchError(pub ListingDiscoveryError);

/// Async `Pipe` that pulls listing only while the coordinator has a free slot.
pub struct DispatchPipe<'call, 'buffer, 'slots, T, const CAPACITY: usize>(
    PhantomData<(
        &'call mut ArtifactFramer<'buffer>,
        &'slots mut SessionSlots<T, CAPACITY>,
    )>,
);

impl<'call, 'buffer, 'slots, T, const CAPACITY: usize>
    DispatchPipe<'call, 'buffer, 'slots, T, CAPACITY>
{
    /// Creates one coordinator pull step.
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T, const CAPACITY: usize> Default for DispatchPipe<'_, '_, '_, T, CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'call, 'buffer, 'slots, T, const CAPACITY: usize> Pipe
    for DispatchPipe<'call, 'buffer, 'slots, T, CAPACITY>
{
    type In = DispatchInput<'call, 'buffer, 'slots, T, CAPACITY>;
    type Out = DispatchStep<'call>;
    type Err = DispatchError;

    async fn call(&self, input: Self::In) -> Result<Self::Out, Self::Err> {
        if input.sessions.is_full() {
            return Ok(DispatchStep::Full);
        }
        let discovery = ListingDiscoveryPipe::new();
        let output = discovery
            .call(ListingDiscoveryInput {
                framer: input.framer,
                chunk: input.chunk,
                max_line_bytes: input.max_line_bytes,
            })
            .await
            .map_err(DispatchError)?;
        Ok(match output.line {
            Some(ListingLine::Test(name)) => DispatchStep::Test {
                name,
                consumed_bytes: output.consumed_bytes,
            },
            Some(ListingLine::Ignored) => DispatchStep::Ignored {
                consumed_bytes: output.consumed_bytes,
            },
            None => DispatchStep::NeedMore {
                consumed_bytes: output.consumed_bytes,
            },
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::Cell;
    use core::future::Future;
    use core::hint::black_box;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use std::alloc::System;
    use std::vec::Vec;

    use super::{DispatchInput, DispatchPipe, DispatchStep};
    use crate::artifact_stream::ArtifactFramer;
    use crate::session_slots::SessionSlots;
    use proxima_primitives::pipe::Pipe;

    struct CountingAllocator;

    std::thread_local! {
        static COUNT_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
        static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if COUNT_ALLOCATIONS.try_with(Cell::get).ok() == Some(true) {
                let _ = ALLOCATION_COUNT.try_with(|count| {
                    count.set(count.get().saturating_add(1));
                });
            }
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) };
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            if COUNT_ALLOCATIONS.try_with(Cell::get).ok() == Some(true) {
                let _ = ALLOCATION_COUNT.try_with(|count| {
                    count.set(count.get().saturating_add(1));
                });
            }
            unsafe { System.realloc(pointer, layout, size) }
        }
    }

    pub(crate) fn measure_allocations<T>(action: impl FnOnce() -> T) -> (T, usize) {
        ALLOCATION_COUNT.with(|count| count.set(0));
        COUNT_ALLOCATIONS.with(|enabled| enabled.set(true));
        let output = action();
        COUNT_ALLOCATIONS.with(|enabled| enabled.set(false));
        let allocations = ALLOCATION_COUNT.with(Cell::get);
        (output, allocations)
    }

    fn run<'call, 'buffer, 'slots, T, const CAPACITY: usize>(
        framer: &'call mut ArtifactFramer<'buffer>,
        sessions: &'slots mut SessionSlots<T, CAPACITY>,
        chunk: &'call [u8],
    ) -> DispatchStep<'call> {
        let pipe = DispatchPipe::<T, CAPACITY>::new();
        let future = pipe.call(DispatchInput {
            framer,
            sessions,
            chunk,
            max_line_bytes: 64,
        });
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match Future::poll(future.as_mut(), &mut context) {
            Poll::Ready(Ok(step)) => step,
            Poll::Ready(Err(error)) => panic!("valid listing line rejected: {error:?}"),
            Poll::Pending => panic!("sans-IO dispatch pipe unexpectedly yielded"),
        }
    }

    #[test]
    fn stops_consuming_listing_when_live_child_slots_are_full() {
        let mut buffer = [0; 65];
        let mut framer = ArtifactFramer::new(&mut buffer, 64).expect("fixed listing capacity");
        let mut sessions = SessionSlots::<&str, 1>::new();
        assert_eq!(
            run(&mut framer, &mut sessions, b"alpha: test\n"),
            DispatchStep::Test {
                name: "alpha",
                consumed_bytes: 12,
            }
        );
        let active = sessions.insert("alpha child").expect("one child slot");
        assert_eq!(
            run(&mut framer, &mut sessions, b"beta: test\n"),
            DispatchStep::Full
        );
        assert!(!framer.frame_ready().expect("listing state remains valid"));
        assert_eq!(sessions.get(active), Some(&"alpha child"));
        std::println!(
            "{{\"active_child\":\"alpha child\",\"consumed_bytes\":0,\"event\":\"pull_while_full\",\"listing_record\":\"beta: test\\n\",\"result\":\"Full\"}}"
        );
    }

    #[test]
    fn resumes_listing_after_child_slot_is_retired() {
        let mut buffer = [0; 65];
        let mut framer = ArtifactFramer::new(&mut buffer, 64).expect("fixed listing capacity");
        let mut sessions = SessionSlots::<&str, 1>::new();
        let active = sessions.insert("alpha child").expect("one child slot");
        assert_eq!(
            run(&mut framer, &mut sessions, b"beta: test\n"),
            DispatchStep::Full
        );
        assert_eq!(sessions.remove(active), Some("alpha child"));
        assert_eq!(
            run(&mut framer, &mut sessions, b"beta: test\n"),
            DispatchStep::Test {
                name: "beta",
                consumed_bytes: 11,
            }
        );
        std::println!(
            "{{\"borrowed_name\":\"beta\",\"consumed_bytes\":11,\"event\":\"pull_after_retire\",\"listing_record\":\"beta: test\\n\",\"result\":\"Test\"}}"
        );
    }

    #[test]
    fn dispatch_pipe_does_not_allocate_and_probe_detects_allocation() {
        let mut buffer = [0; 65];
        let mut framer = ArtifactFramer::new(&mut buffer, 64).expect("fixed listing capacity");
        let mut sessions = SessionSlots::<&str, 1>::new();
        ALLOCATION_COUNT.with(|count| count.set(0));
        COUNT_ALLOCATIONS.with(|enabled| enabled.set(true));
        let output = run(&mut framer, &mut sessions, b"beta: test\n");
        COUNT_ALLOCATIONS.with(|enabled| enabled.set(false));
        let dispatch_allocations = ALLOCATION_COUNT.with(Cell::get);

        ALLOCATION_COUNT.with(|count| count.set(0));
        COUNT_ALLOCATIONS.with(|enabled| enabled.set(true));
        let allocation_control = Vec::<u8>::with_capacity(1);
        COUNT_ALLOCATIONS.with(|enabled| enabled.set(false));
        let positive_control_allocations = ALLOCATION_COUNT.with(Cell::get);
        black_box(allocation_control);

        assert_eq!(
            output,
            DispatchStep::Test {
                name: "beta",
                consumed_bytes: 11,
            }
        );
        assert_eq!(dispatch_allocations, 0);
        assert_eq!(positive_control_allocations, 1);
        std::println!(
            "{{\"dispatch_allocations\":{dispatch_allocations},\"event\":\"allocation_probe\",\"positive_control_allocations\":{positive_control_allocations}}}"
        );
    }
}
