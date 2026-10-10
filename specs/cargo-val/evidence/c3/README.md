# C3 / AC65 artifact framing evidence

The literal sequence in `oracles/c3.jsonl` supplies two Cargo test-artifact
records over three chunks, then an EOF event. `validate_c3_oracle.py` reports
two ordered paths, one first-emission checkpoint while the producer is open,
two retained Cargo artifact controls, and one rejection for each reversed
ordering and missing-newline control. The retained machine output is
`oracle-validation.jsonl`. `degenerate-control.jsonl` records a separately
mutated missing-newline input: it produced zero first-emission paths and did
not match the expected held-open checkpoint.

`implementation-test.txt` records the exact focused test invocation. It emits
one first path after the terminating newline arrives while the test producer
is still open, then emits the second path after its newline. A partial line
returns `None`. The test's operation order holds EOF until after the first
selection; it does not use a sleep or a process thread. The test output reports
one selected test passed, with six additional boundary/progress tests in the
full target.

The adapter in `src/artifact_stream.rs` calls Proxima's borrowed
`DelimiterCodec::parse_frame` and the C2 parser `parse_artifact`. A fixed
caller-provided byte slice bounds accumulation; `push` reports how many chunk
bytes it consumed so the caller retains any unconsumed tail until it drains a
frame. The adapter keeps an enum scan cursor so each newly received byte is
checked once for the newline, then asks Proxima `DelimiterCodec` to validate a
complete frame. This retains the incremental scan shape of Proxima's
`DelimiterFraming` while keeping the buffer caller-owned and fixed-capacity;
the owned primitive uses growing `BytesMut` storage. The C3 output is a
borrowed path into the input record or caller scratch, carried through
Proxima's no_std `Pipe` trait. The parser and framing storage allocate no
payload buffers in these paths by construction. The focused macOS execution
emitted `C3_ALLOCATION_TRACE` with two records, zero allocator calls during
construction and polling of the three `ArtifactPipe` futures (including
framing and parsing), and
one call in the deliberate `Vec::with_capacity(1)` positive control. The full
seven-test target is run with one test thread because its process-global
allocator counter observes allocations made concurrently by sibling tests.
The parallel run in `parallel-allocation-control.txt` passed six tests and
failed the measured test's `0` allocation assertion with `5` counted calls.
The serial run in `all-framing-tests.txt` passed all seven, and the focused
serial run in `implementation-test.txt` counted zero allocator calls in the parser path and one
positive-control call. Only test-thread concurrency changed between the two
full-target runs. The counter is process-global
(`tests/cargo_artifact_framing.rs:20-44`) and its enabled windows surround the
measured calls (`:118-126`, `:144-153`, `:177-185`); the assertion is at `:217`.
The parallel record does not identify which sibling allocation sites
contributed to the five calls. The complete claim record
and source pointer are in `allocation-trace.jsonl`. A one-byte-chunk framing test feeds a
record one byte at a time and checks that the partial record stays buffered until its
delimiter arrives. Ignored complete records set `record_consumed`, allowing a
caller to drain further buffered records without inferring progress from an
absent selection. A short-consume test replays the exact unconsumed input tail
and observes the second ordered executable. A parse-failure test proves later
push and EOF calls report the terminal stream error. This is one host and one literal
two-record stream; it does not establish the Windows or Linux allocation
count.

`nostd-library-check.txt` records the library check with both Proxima
dependencies enabled and cargo-val default features disabled. The matching
`feature-tree.txt` is restricted to normal dependencies and feature edges; it
contains zero `feature "std"` entries. It still contains `alloc` feature edges
from Proxima primitives' dependencies, even though cargo-val does not enable
Proxima codec's `alloc` feature and the measured C3 calls make zero allocator
calls. Build-script dependencies are excluded from the shipped runtime graph.
This check does not establish that the final binary or child-session
implementation is no_std.

Remaining C3 evidence: the retained implementation test models a live producer
by withholding EOF in a deterministic call sequence; it does not exercise an
OS stdout source. Native Cargo child streaming remains downstream work.
