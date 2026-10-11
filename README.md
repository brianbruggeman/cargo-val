# cargo-val

A Proxima-based Cargo test runner, invoked as `cargo val`.

Install the runner from this checkout:

```sh
cargo install --path . --locked
```

Cargo discovers the installed `cargo-val` executable as the `cargo val`
subcommand. Check the command with `cargo val --help`, then run a package's test
targets with `cargo val --tests`. The runtime is `no_std` and installs a
rejecting global allocator. Its async driver starts each selected test process
as listing records arrive and writes colored results unless `NO_COLOR` is set.

The current command path is connected for macOS and GNU/Linux. Windows host
primitives and native fixtures exist, but the Windows runner is not connected
to the executable yet. Performance evidence is workload-dependent: see
[`specs/cargo-val/TASKS.md`](specs/cargo-val/TASKS.md)
and the retained per-run records under `specs/cargo-val/evidence/c54b/`.

The implementation pins Proxima and conflaguration Git revisions and reuses
Proxima's no_std pipe, async read, and framing primitives. It does not modify
the Proxima repository.

## Selection, ignored tests and summaries

- `cargo val -E 'package(proxima-*) and not test(=bind::tests::slow)'` selects with a
  subset of nextest's filterset language (`test() binary() package() all() none()`,
  `and & or | + not ! -`, parentheses; matchers contains, `=exact`, `#glob`). It is
  applied while listing, so excluded tests are never launched and count as `filtered`.
  Regex matchers are refused: the runner is `no_std` with no heap. See `-h`.
- `#[ignore]`d tests are listed (`--list --ignored`), not launched, and reported
  `IGNORED`. `-- --ignored` runs only them and `-- --include-ignored` runs everything.
  A child is classified by its libtest result line, so zero executed bodies never reads
  as PASS.
- The run ends with `<binary>: discovered N, passed P, failed F, ignored I, filtered X`
  for each test binary and a `total:` line with the wall time.
- Integration tests and benches wait until their package's `bin` targets are built.
