# C45b Windows overlapped buffer lifetime

## Observation register

- Microsoft Learn `ReadFile`, lines 54-58: the supplied read buffer remains in use for the duration of the operation and must not be used before completion.
- Microsoft Learn `ReadFile`, lines 74-76: an overlapped handle requires a valid, unique `OVERLAPPED` structure.
- Microsoft Learn `ReadFile`, lines 118-120 and 129-135: completion can remain pending; completion-port results report transferred bytes; the operation state cannot be modified, freed, or reused until completion.
- Microsoft Learn `CancelIoEx`, lines 55-63: requesting cancellation does not wait for completion; the associated `OVERLAPPED` cannot be freed or reused before completion, and a still-pending asynchronous operation queues a completion packet.
- Pinned Proxima `AsyncRead`, `proxima-core/src/io.rs:57-68`: `poll_read` receives a mutable buffer borrow for the duration of one poll call and returns `Poll::Pending` without retaining that borrow in its type contract.
- cargo-val `HostEventPipe`, `src/host_event.rs:49-55,70-88`: the existing pipe event accepts borrowed output bytes and returns them without taking ownership.

## Derived implementation constraint

The Windows read buffer and `OVERLAPPED` must live in stable session storage from submission through completion, including cancellation completion. A pending `ReadFile` cannot safely target the transient `poll_read` buffer. After completion, expose the session buffer as a borrowed slice to the existing `HostEventPipe`; do not add an intermediate payload copy, semaphore, permit, or waiter queue.

## Primary sources

- https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-readfile
- https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-cancelioex
