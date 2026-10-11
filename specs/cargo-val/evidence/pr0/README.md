# PR0 alloc-tier semaphore evidence

## Execution environment

- Host: `aarch64-apple-darwin`, macOS kernel `Darwin 24.6.0`.
- Rust: `rustc 1.99.0 (b940084d7 2026-09-28)`.
- Proxima source: commit `cfefabe359614e2b92c839166fd1fbf5af5ac781` (`feat: expose async semaphore in alloc tier`), on `main` and pushed to `origin`.
- Oracle: `../oracles/pr0.jsonl`, SHA-256 `c8011483d4deb8d0102a86b66d5bc4b1f302b8bece0f5dca2bebce6f7785bd1d`.

## Commands and payloads

| command | observed record | raw artifact |
|---|---|---|
| `cargo check -p proxima-primitives --no-default-features --features alloc --lib --message-format=json` | 1 `proxima_primitives` library artifact, 0 compiler errors, build-finished success | [check.jsonl](check.jsonl); stderr [check.stderr](check.stderr) |
| `cargo test -p proxima-primitives --no-default-features --features alloc --lib sync::semaphore::tests::release_wakes_one_pending_acquire -- --exact` | `running 1 test`; 1 passed, 0 failed; 427 filtered | [release-test.txt](release-test.txt) |
| `cargo test -p proxima-primitives --no-default-features --features alloc --lib sync::semaphore::tests::close_wakes_pending_acquires -- --exact` | `running 1 test`; 1 passed, 0 failed; 427 filtered | [close-test.txt](close-test.txt) |
| `cargo tree -p proxima-primitives --no-default-features --features alloc -e normal,features -i async-lock --depth 3` | `async-lock` is enabled by `proxima-primitives/alloc`; no `std` feature appears on this normal dependency path | [async-lock-tree.txt](async-lock-tree.txt); source feature wiring is [Cargo.toml](/Users/brianbruggeman/repos/slot-0/proxima-windows/proxima-primitives/Cargo.toml:21) |

The release test asserts Pending and zero wakes while its permit is held, then one wake and Ready(Ok) after release. The close test asserts Pending and zero wakes before close, one close wake and Ready(Err), then zero extra wakes when the held permit is released. Source assertions are at `proxima-primitives/src/sync/semaphore.rs:253-295`.

## Allocation observation

`Semaphore::acquire` calls `close_event.listen()` before checking for an available permit at `proxima-primitives/src/sync/semaphore.rs:78-87`. The dependency source documents that `Event::listen()` heap-pins a listener at `event-listener-5.4.1/src/lib.rs:300-318`. This is source evidence for an allocation path, not a measured runtime allocation count. Measure this path before changing its design or claiming allocation reduction.
