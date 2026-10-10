# C5 implementation trace

The first pipe implementation called `ArtifactFramer::next_frame()` to decide
whether it could push the next chunk. The returned `Option<&[u8]>` borrow
remained live through the returned output, so the push branch attempted a
second mutable borrow. Rustc reported E0499 at both the `push` and later
`next_frame` calls in `src/discovery_stream.rs` (lines 73-74 at the time).

The final adapter uses `frame_ready()` to read the FSM state without yielding a
slice, pushes only when no frame is ready, then calls `peek_frame()` and passes
that borrow to the existing libtest parser Pipe. The framer's cursor and source
state use caller-owned `Cell` fields, so `consume_frame()` or `poison()` updates
only FSM state while preserving the test-name borrow into the fixed buffer.
This abandoned the borrow-returning mutable peek branch and kept one bounded
frame as the data owner.

The first review also found lost accepted-byte counts on parser error, a
framing adapter that remained usable after malformed input, and an EOF path
that could skip nonempty input validation when a frame was buffered. The
correction carries `consumed_bytes` in discovery errors, poisons after parser
rejection, validates chunks before processing ready frames, accepts empty
polls after EOF, and rejects nonempty post-EOF input. Tests assert each state
transition.

A boundary run split `alpha: test\n` into one-byte chunks and observed no
output before the newline, then one borrowed name. A second run buffered two
lines and supplied a later chunk while the second frame was pending; the
adapter returned the second name with `consumed_bytes=0`, then replaying the
same chunk emitted the third name. These cases are in
`withholds_split_line_until_newline_arrives` and
`drains_buffered_line_before_accepting_new_chunk`.

The failed command was `CARGO_TARGET_DIR=/tmp/cargo_val_c3_target cargo test
-p cargo-val --no-default-features --features
proxima-codec,proxima-primitives --test discovery_stream --
--test-threads=1`. Rustc printed `error[E0499]: cannot borrow *input.framer as
mutable more than once at a time` for `push` and the second `next_frame` call.
