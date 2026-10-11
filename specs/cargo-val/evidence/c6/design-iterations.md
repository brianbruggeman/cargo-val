# c6 admission design and observed poll records

The pinned Proxima `Pipe` call has `In = Source::In`,
`Out = Admission<Source::In, Source::Out>`, and `Err = Source::Err` in
`src/admission.rs`. `AdmissionPipe::call` first attempts
`AtomicPermitPool::try_acquire`. If capacity is full it returns
`Deferred(input)` without calling the source. On success it awaits exactly one
source call and returns the RAII permit with that output. A source error drops
the permit on the error path.

The async host loop owns the deferred input. A child completion drops its
permit; the next loop transition retries the same input. No waiter is parked,
and no discovered test is queued. This differs from `Demand`, which discards
input while disarmed, and from an async semaphore waiter, which adds a waiter
queue that the event loop does not need.

The direct-Proxima control calls the same `try_acquire` and source in the same
order. `acceptance-records.jsonl` contains the literal candidate, direct
control, and zero-capacity records from `run_c6_acceptance.py`. The current
serial macOS debug run observed matching poll allocation deltas of zero for
first, second, deferred, and retry polls in both arms. The retained positive
control observed one allocation. These records cover only the measured test
polls; they do not measure child process allocations or establish whole-runner
allocation behavior.

The abandoned designs were a semaphore wait queue and a bounded name queue.
The first duplicates wakeup ownership already held by the async host loop. The
second reads names before process capacity exists and must then add storage and
overflow handling. The current slice uses existing Proxima primitives only;
it does not add code to the Proxima repository.
