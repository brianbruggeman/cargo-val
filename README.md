# cargo-val

A Proxima-based Cargo test runner, invoked as `cargo val`.

The package name `cargo-val` is available on crates.io as checked on
2026-10-09. The package `cargo-validate` already exists and provides the
`cargo-validate` executable, so this project uses the distinct `cargo val`
subcommand. The design and implementation cards are in
[`specs/cargo-val/`](specs/cargo-val/).

The current help-only executable is a temporary scaffold and uses `std`; it
does not meet the design. The shipped binary and production dependency graph
must be no_std, and process execution must use Proxima's async child-session
path on Windows, macOS, and Linux. That requires no_std configuration support
from conflaguration and reactor-driven process-session support from Proxima.
The Proxima crates checked for this design are not published on crates.io, so
integration must pin a Proxima Git revision rather than use a local path.
