# C4 / AC66 libtest listing line

The independent oracle in `oracles/c4.jsonl` has one test-name line, one
summary line, and one name with a mutated suffix. `validate_c4_oracle.py`
compiles the separate libtest fixture, runs its real `--list` output, and
compares both lines with the literal records. Its retained counts are in
`oracle-validation.jsonl`.

`src/libtest_listing.rs` accepts one complete line, uses Proxima's borrowed
newline `DelimiterCodec` and `Pipe`, and returns a borrowed test name only for
the exact `: test` suffix. Summary and changed-suffix lines return `Ignored`.
The caller supplies the complete-line byte cap; the parser rejects longer
inputs before framing. It uses no `std`, allocation, or lock APIs. The focused
test in `implementation-test.txt` covers the name, summary, suffix,
missing-newline, trailing-bytes, invalid-UTF-8, empty-name, over-limit, and CRLF
cases. It checks pointer identity for the borrowed name, observed zero
allocator calls in nine parser calls, and detected one deliberate
`Vec::with_capacity(1)` allocation. Assertions are in
`tests/libtest_listing.rs:52-135`.

`nostd-library-check.txt` records the exact library-check command with default
features off.
The `cargo-val` parser module is `no_std`. Proxima's normal dependency graph
still carries `alloc` feature edges from `proxima-primitives` dependencies;
the parser source and measured calls do not allocate. This slice does not
discover names from a running child process; C5 owns streaming discovery.
