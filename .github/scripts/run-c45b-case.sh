#!/usr/bin/env bash

set +e
set -o pipefail

cargo test \
  --no-default-features \
  --features proxima-codec,proxima-primitives,proxima-core,edge-os \
  --lib "$1" \
  -- --exact --nocapture --test-threads=1 2>&1 |
  awk '
    {
      print $0
      fflush()

      if (index($0, "\"case\":\"c45b\"") > 0) {
        stage = $0
        sub(/^.*"stage":"/, "", stage)
        sub(/".*$/, "", stage)
        print "::notice title=c45b stage::" stage
        fflush()
      }
    }
  '

test_status=$?
exit "$test_status"
