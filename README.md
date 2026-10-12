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

The command path is connected for macOS, GNU/Linux, and Windows. The Windows
edges (completion-port reads, process-exit and Ctrl+C gates, one job object per
run) are exercised by the native `windows-latest` jobs in
`.github/workflows/native-tests.yml`. Performance evidence is workload-dependent: see
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

## GitHub Action

Any repository can run its tests with cargo val. The caller installs the Rust
toolchain first (the action does not); the runner installs on Linux and macOS.

```yaml
steps:
  - uses: actions/checkout@v4
  - uses: dtolnay/rust-toolchain@stable
  - uses: brianbruggeman/cargo-val@main
    with:
      filterset: 'package(my-crate)'
      no-fail-fast: true
```

The action resolves `version` to a commit with `git ls-remote`, restores a cached
binary keyed on runner OS, architecture and that commit, runs `cargo install
--locked --git` on a miss, runs `cargo val --tests`, writes the `total:` line to
the job summary and fails the step when cargo val fails.

| input | default | meaning |
| --- | --- | --- |
| `version` | the action's own ref, else `main` | git ref (branch, tag or full sha) of cargo-val to install |
| `args` | empty | extra arguments for `cargo val`, split on whitespace |
| `working-directory` | `.` | directory cargo val runs in |
| `filterset` | empty | passed as `-E` |
| `no-fail-fast` | `false` | passed as `--no-fail-fast` |

Outputs: `total` (the `total:` line) and `sha` (the installed commit).
