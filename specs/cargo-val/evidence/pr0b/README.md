# PR0b macOS allocation fast path

Source: Proxima worktree `pr0a-semaphore-allocation-probe`, with the PR0b diff under review. Host and compiler are recorded in `host.txt`. Raw alloc-only Cargo JSON and compiler stderr are `alloc-check.jsonl` and `alloc-check.stderr`; exact allocation and lifecycle command output is `tests-macos.txt`.

The alloc-only check emitted one `proxima_primitives` library artifact and zero compiler error events across 102 Cargo JSON messages. The allocation test passed once and emitted six records with deltas 0, 1, 0, 0, 0, 0. The close-before-acquire, close-wakes-pending, and release-wakes-pending tests each passed once.

This is native macOS evidence for an uncontended path. Linux and Windows runs and contended allocation counts are unobserved. The fast path uses the existing `try_acquire`; when no permit is available, `acquire` retains the existing listener/check/select path.
