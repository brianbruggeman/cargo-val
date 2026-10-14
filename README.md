# cargo-val

A Proxima-based Cargo test runner, invoked as `cargo val`.

![cargo val on a failing workspace: live frame, pinned failure, review screen, receipt](https://raw.githubusercontent.com/brianbruggeman/cargo-val/main/assets/demo.gif)

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
`.github/workflows/native-tests.yml`. Performance evidence is workload-dependent.

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
toolchain first (the action does not); the action runs on Linux, macOS and Windows runners.

```yaml
steps:
  - uses: actions/checkout@v4
  - uses: dtolnay/rust-toolchain@stable
  - uses: brianbruggeman/cargo-val@main
    with:
      filterset: 'package(my-crate)'
      no-fail-fast: true
```

`version` selects the source. A semver version (`0.1.0`, `v0.1.0`, `0.1`) installs that
release from crates.io with `cargo install --locked cargo-val --version`, cached per
version. Anything else is a git ref: it is resolved to a commit with `git ls-remote`,
installed with `cargo install --locked --git`, and cached per commit. With the default,
`uses: brianbruggeman/cargo-val@v0.1.0` installs the registry 0.1.0 while `@main` or a
sha installs from git. The action then runs `cargo val --tests`, writes the `total:`
line to the job summary and fails the step when cargo val fails. On Windows, pass
`args: -- --test-threads=1` when the suite asserts process-wide handle counts.

| input | default | meaning |
| --- | --- | --- |
| `version` | the action's own ref, else `main` | semver version (installed from crates.io) or git ref (branch, tag or full sha, installed from GitHub) |
| `args` | empty | extra arguments for `cargo val`, split on whitespace |
| `working-directory` | `.` | directory cargo val runs in |
| `filterset` | empty | passed as `-E` |
| `no-fail-fast` | `false` | passed as `--no-fail-fast` |

Outputs: `total` (the `total:` line) and `sha` (the installed commit; empty for a crates.io install).
