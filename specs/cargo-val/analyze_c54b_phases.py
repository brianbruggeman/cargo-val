#!/usr/bin/env python3
"""Derive auditable C54b spans from the raw C54PHASE records."""

import json
from pathlib import Path
import statistics
import sys


def spans(events: list[tuple[str, int, int, int]], start: str, end: str) -> list[int]:
    beginnings = [(stamp, process_id) for kind, stamp, process_id, _ in events if kind == start]
    endings = [(stamp, process_id) for kind, stamp, process_id, _ in events if kind == end]
    result = []
    for stamp, process_id in beginnings:
        matching = next((finish for finish, child in endings if child == process_id and finish >= stamp), None)
        if matching is None:
            raise AssertionError(f"{start} process {process_id} has no {end}")
        result.append(matching - stamp)
    return result


def analyze(directory: Path) -> dict:
    records = []
    raw_path = directory / "paired-runs.raw.jsonl"
    for line_number, line in enumerate(raw_path.read_text().splitlines(), 1):
        command = json.loads(line)
        if command["runner"] != "cargo_val" or command["round"] == 0:
            continue
        events = []
        for stderr_line in command["stderr"].splitlines():
            if stderr_line.startswith("C54PHASE "):
                _, kind, stamp, process_id, count = stderr_line.split()
                events.append((kind, int(stamp), int(process_id), int(count)))
        labels = [kind for kind, _, _, _ in events]
        starts = [stamp for kind, stamp, _, _ in events if kind == "run_start"]
        completed = [stamp for kind, stamp, _, _ in events if kind == "run_done"]
        if len(starts) != 1 or len(completed) != 1:
            raise AssertionError(f"run {command['round']} lacks one start and completion")
        if labels.count("test_spawn") != 17 or labels.count("test_exit") != 17:
            raise AssertionError(f"run {command['round']} does not have 17 child starts and exits")
        waits = spans(events, "list_full", "list_resume")
        test_spans = spans(events, "test_spawn", "test_exit")
        listing_spans = spans(events, "listing_spawn", "listing_done")
        first_test = next(stamp for kind, stamp, _, _ in events if kind == "test_spawn")
        last_listing = max(stamp for kind, stamp, _, _ in events if kind == "listing_done")
        record = {
            "round": command["round"],
            "raw_source": f"{raw_path}:{line_number}",
            "wall_ns": command["elapsed_ns"],
            "trace_run_ns": completed[0] - starts[0],
            "first_test_after_run_start_ns": first_test - starts[0],
            "final_drain_after_last_listing_ns": completed[0] - last_listing,
            "listing_spans_ns": listing_spans,
            "list_full_wait_spans_ns": waits,
            "list_full_wait_sum_ns": sum(waits),
            "test_spawn_to_exit_ns": test_spans,
            "test_spawn_count": labels.count("test_spawn"),
            "test_exit_count": labels.count("test_exit"),
            "listing_spawn_count": labels.count("listing_spawn"),
            "listing_done_count": labels.count("listing_done"),
        }
        records.append(record)
    if len(records) != 5:
        raise AssertionError(f"expected 5 paired cargo-val records, got {len(records)}")
    summary = {
        "runs": records,
        "median_wall_ns": statistics.median(record["wall_ns"] for record in records),
        "median_list_full_wait_sum_ns": statistics.median(record["list_full_wait_sum_ns"] for record in records),
        "median_first_test_after_run_start_ns": statistics.median(record["first_test_after_run_start_ns"] for record in records),
        "median_final_drain_after_last_listing_ns": statistics.median(record["final_drain_after_last_listing_ns"] for record in records),
        "cargo_exit_observation": "the outer coordinator defers Cargo reaping during nested listing; cargo_spawn-to-cargo_exit is not Cargo build duration",
    }
    (directory / "phase-summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return summary


def main() -> None:
    for argument in sys.argv[1:]:
        directory = Path(argument)
        summary = analyze(directory)
        print(json.dumps({"directory": str(directory), "runs": len(summary["runs"]), "median_wall_ns": summary["median_wall_ns"], "median_list_full_wait_sum_ns": summary["median_list_full_wait_sum_ns"], "median_first_test_after_run_start_ns": summary["median_first_test_after_run_start_ns"], "median_final_drain_after_last_listing_ns": summary["median_final_drain_after_last_listing_ns"]}))


if __name__ == "__main__":
    main()
