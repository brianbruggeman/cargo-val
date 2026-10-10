# C5 / AC67 incremental test discovery

`ListingDiscoveryPipe` composes the existing bounded `ArtifactFramer` with
`LibtestFramePipe`. Each call validates stream state, accepts bytes only when
no complete frame is buffered, emits at most one borrowed test name, and
reports the copied-byte count so the caller can replay an unconsumed chunk
tail. The caller dispatches the name while holding its borrow, then resumes
discovery. No suite vector or name queue is constructed in the production
source.

The literal stream in `oracles/c5.jsonl` has one first test line, three later
test lines, then EOF. The integration test uses a held-open producer that
returns `Pending` until the downstream dispatcher consumes the first name;
only then does it release later names and eventually EOF. It also checks
summary progress, oversized-line byte progress, bytewise delimiter splits,
multi-line tail replay, malformed-line byte accounting and poisoning, and
empty/nonempty polling after EOF.

`implementation-test.txt` records 9 passing tests. `nextest-control.txt` runs the same gated four-name outcome test through cargo-nextest and records 1 passed, 8 filtered. `nostd-library-check.txt`
records the library build with default features disabled. `oracle-validation.jsonl`
records 1 gated dispatch before EOF, 3 subsequent names, 4 ordered literal
names, 1 EOF after the names, and 0 production suite-vector tokens.
`src/discovery_stream.rs` is the bounded adapter;
`tests/discovery_stream.rs:34-133` contains the gated producer and dispatcher.
