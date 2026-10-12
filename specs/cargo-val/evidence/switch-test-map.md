# switch: removed tests mapped to the tests that cover them

Every test removed while the hand-written wait loop moved onto Prime, with the test that now carries it.
Removed == mapped on every row.

| removed | mapped to | what the mapped test asserts |
|---|---|---|
| `host::unix_rustc::tests::host_libdir_is_the_directory_that_holds_libstd` | `run::tests::host_libdir_is_the_directory_that_holds_libstd` | the printed libdir exists and holds `libstd-*`, collected through a Prime lane |
| `host::unix_rustc::tests::start_returns_while_rustc_is_still_running_and_finish_collects_its_output` | `run::tests::spawn_returns_while_rustc_is_still_running_and_prime_collects_its_output` | the child is still running when spawn returns, and its output arrives after the gate opens |
| `host::unix_rustc::tests::unknown_target_is_reported_as_an_output_failure` | `run::tests::unknown_target_is_reported_as_an_output_failure` | an unknown triple yields `RunError::RustcLibdir` |
| `cargo_source::tests::c49_native_child_stdout_yields_before_the_child_closes_it` (macos) | `edge_stream_unix::tests::c49_native_child_stdout_yields_before_the_child_closes_it` | a child's stdout yields bytes while the child still holds the pipe open, over `ChildStream` |

## module moves out of `host::` (same test, new path)

Fifteen tests changed module path only; the removed list and the mapped list were diffed from `cargo test --lib -- --list`, 15 removed == 15 mapped, lib total 188 before and after.

| removed | mapped to |
|---|---|
| `host::unix_driver::tests::*` (8 tests) | `unix_driver::tests::*` (same 8 names) |
| `host::unix_env::tests::*` (2 tests) | `unix_env::tests::*` (same 2 names) |
| `host::unix_pipe_pair::tests::*` (2 tests) | `unix_pipe_pair::tests::*` (same 2 names) |
| `host::unix_spawn::tests::*` (3 tests) | `unix_spawn::tests::*` (same 3 names) |

`host/macos_exit.rs`, `host/linux_exit.rs`, `host/macos_pipe.rs`, `host/linux_pipe.rs` and `host/unix_rustc.rs` carried no tests that remain; `spawn_libdir_query` moved into `unix_spawn.rs` and returns `UnixSpawnError { code: EINVAL }` for oversize arguments instead of a separate error enum.
