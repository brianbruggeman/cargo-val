#!/usr/bin/env python3
"""Assert every raw lookahead repetition reached all exact child results."""

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "specs/cargo-val/evidence/c54b/lookahead-complete-repetitions.raw.jsonl"


def main() -> None:
    records = [json.loads(line) for line in SOURCE.read_text().splitlines()]
    if len(records) != 300:
        raise AssertionError(f"expected 300 raw invocations, got {len(records)}")
    for expected_index, record in enumerate(records, 1):
        labels = record["labels"]
        if (
            record["index"] != expected_index
            or record["status"] != 0
            or len(labels) != 17
            or len(set(labels)) != 17
            or any(not label.endswith(" PASS") for label in labels)
        ):
            raise AssertionError(f"invocation {expected_index} was incomplete: {record}")
    print(json.dumps({
        "raw_invocations": len(records),
        "complete_17_pass_invocations": len(records),
        "exit_zero_invocations": len(records),
        "minimum_elapsed_ns": min(record["elapsed_ns"] for record in records),
        "maximum_elapsed_ns": max(record["elapsed_ns"] for record in records),
        "source": str(SOURCE),
    }))


if __name__ == "__main__":
    main()
