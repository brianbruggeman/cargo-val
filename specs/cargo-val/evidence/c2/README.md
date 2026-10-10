# C2 Cargo artifact record

Host: Darwin arm64. The retained external control is
`cargo-all-targets.jsonl`, captured from:

```sh
cargo test --no-run --message-format=json
```

It contains three compiler-artifact records: a binary test harness, a normal
binary, and an integration test executable. The hand-authored literal oracle
`oracles/c2.jsonl` preserves those three shapes. Selection is based on
`reason=compiler-artifact`, `profile.test=true`, and a nonempty executable.
It does not depend on `target.test` or `target.kind`.

The independent oracle command was:

```sh
CARGO_TARGET_DIR=/tmp/cargo_val_trace_target python3 specs/cargo-val/validate_c2_oracle.py
```

Its raw result is `oracle-validation-latest.jsonl`:

```json
{"borrowed_path_controls": 1, "escaped_field_spellings": 1, "escaped_windows_decoded_controls": 1, "literal_non_test_ignored": 1, "literal_test_executables": 2, "live_cargo_non_test_artifacts": 1, "live_cargo_test_artifacts": 2, "naive_selectors_rejected": 2, "over_byte_probe_shapes": 1, "over_depth_probe_shapes": 1, "raw_cargo_non_test_artifacts": 1, "raw_cargo_test_artifacts": 2, "short_scratch_probe_shapes": 1}
```

`validate_c2_oracle.py` compares literal, retained Cargo, and live Cargo record
shapes, and independently decodes the escaped-key fixture in
`oracles/c2-escaped-fields.jsonl` against its expected record. Its negative selectors verify that using only `target.test` selects
the normal binary too, while using only `target.kind=["test"]` misses the
binary test harness. A corrupted decoded-path expectation is rejected by the
validator. The script verifies oracle shape; it does not measure Rust parser
allocations.

The implementation is the bounded no_std FSM in `src/lib.rs`. It accepts one
complete Cargo JSON line, caller scratch, a byte limit, and a depth limit
capped at 16. Plain paths borrow the input; escaped paths decode into scratch.
Null executable records are ignored, matching retained Cargo artifacts in
`no-std-check.jsonl:1,3`. It rejects malformed JSON, duplicate recognized
fields, invalid UTF-8, over-byte records, over-depth records, and insufficient
scratch. It performs no runtime allocation. Proxima's current JSON codec is
std-gated (`proxima/proxima-codec/Cargo.toml:30-46` and
`proxima/proxima-codec/src/lib.rs:213-246`), so this specific record FSM stays
in cargo-val. C3 uses Proxima delimiter framing in a fixed-buffer stream
adapter. The caller bounds storage before supplying complete lines; this
parser's per-record limit then validates the supplied record. The OS child
stdout source remains a later process-integration slice.

The allocation probe lives in `tests/artifact_stream.rs`. It deliberately
allocates once to prove the counter is sensitive, then counts parser calls across executable, ignored, and limit cases. Current acceptance outputs are retained
in `parser-test-noalloc.txt`, `no-std-check-latest.txt`, and
`normal-feature-tree-latest.txt`:

| command | observed output |
|---|---|
| `CARGO_TARGET_DIR=/tmp/cargo_val_trace_target cargo test -p cargo-val --no-default-features` | 1 artifact parser test and 1 CLI smoke test passed; library, binary, and doc-test targets completed with 0 failures |
| `CARGO_TARGET_DIR=/tmp/cargo_val_trace_target cargo test -p cargo-val --no-default-features --test artifact_stream` | 1 test passed, 0 failed; 2 original executables, 1 escaped Windows path, and 1 escaped-key selector; 1 production artifact and 1 null executable ignored; 0 allocations across 10 measured parser calls (4 executable, 2 ignored, 4 limit cases); 1 deliberate allocation observed; malformed/trailing-comma/duplicate/invalid-UTF8 inputs rejected |
| `CARGO_TARGET_DIR=/tmp/cargo_val_trace_target cargo check -p cargo-val --lib --no-default-features` | library check completed with 0 compiler errors |
| `cargo tree -p cargo-val --no-default-features -e normal,features` | output contains only `cargo-val`; zero normal runtime dependencies |
| `CARGO_TARGET_DIR=/tmp/cargo_val_trace_target python3 specs/cargo-val/validate_c2_oracle.py` | counts shown above; command exited 0 |

These checks cover the library and its normal dependency graph. They do not
establish the shipped binary's no_std status: `src/main.rs` still uses std,
and C0e-C0g cover that separate entry boundary. C3 streams bounded input from
a caller-owned fixed buffer; a later process card must connect that buffer to
an OS child stdout source.
