# PR0a allocation probe

The retained exploratory run is `semaphore-alloc-macos.txt`. It records the exact command, host output, and six raw allocation records from the committed test source at Proxima commit `91f787b9`. The host is macOS `aarch64-apple-darwin` (Darwin 24.6.0); this does not discharge the card's native Linux acceptance command.

Observed records in that macOS run:

- empty control: 0 allocations
- boxed positive control: 1 allocation
- async acquire: 1 allocation for one cycle and 16 for 16 cycles
- `try_acquire`: 0 allocations for one cycle and 16 cycles

The records locate the measured allocation to the async acquire path in this test setup. They do not establish a Windows result or justify changing Semaphore behavior without the Linux card run and source review.
