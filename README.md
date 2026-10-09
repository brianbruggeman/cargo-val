# cargo-val

A Proxima-based Cargo test runner, invoked as `cargo val`.

The package name `cargo-val` is available on crates.io as checked on
2026-10-09. The package `cargo-validate` already exists and provides the
`cargo-validate` executable, so this project uses the distinct `cargo val`
subcommand. The design and implementation cards are in
[`specs/cargo-val/`](specs/cargo-val/).

This repository is at the design-card stage. The cards require Proxima process
support on Windows before the runner can claim native Windows support. The
Proxima crates checked for this design are not published on crates.io, so the
initial integration must pin a Proxima Git revision instead of using a local
path dependency.
