# C48g focused test launch observation

The focused test command built successfully in `/tmp/cargo_target`, but its
test process produced no harness output. `sample` recorded only `_dyld_start`
and `vmmap` reported “Process exists but has not started -- it is
launched-suspended”; the process footprint was 96K and no malloc zones were
present. This does not identify why the default-target launch remained
suspended.

The same source and command completed in a separate target directory at
`evidence/c48g/library-tests.txt`: two C48g tests passed in 0.00s. That
isolated-target run is the acceptance evidence; the default-target launch cause
remains unexplained.
