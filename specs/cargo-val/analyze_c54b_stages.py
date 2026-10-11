#!/usr/bin/env python3
"""Extract exact child launch stage spans from temporary C54TRACE records."""

import json
from pathlib import Path
import statistics


RAW = Path("specs/cargo-val/evidence/c54b/trace_stages/raw.jsonl")
OUTPUT = RAW.with_name("launch-stages.jsonl")
STAGES = (
    ("argument_environment_ns", "launch_begin", "pipe_begin"),
    ("pipe_creation_ns", "pipe_begin", "pipe_done"),
    ("file_actions_ns", "actions_begin", "actions_done"),
    ("posix_spawn_ns", "spawn_begin", "spawn_done"),
    ("whole_launch_ns", "launch_begin", "launch_return"),
)


def parse_events(stderr: str) -> list[tuple[str, int, int]]:
    events = []
    for line in stderr.splitlines():
        if line.startswith("C54TRACE "):
            _, kind, stamp, value = line.split()
            events.append((kind, int(stamp), int(value)))
    return events


def launch_records(events: list[tuple[str, int, int]]) -> list[dict]:
    records = []
    for index, (kind, stamp, _) in enumerate(events):
        if kind != "launch_begin":
            continue
        ending = next(position for position in range(index + 1, len(events)) if events[position][0] == "launch_return")
        segment = events[index:ending + 1]
        observations = {event: timestamp for event, timestamp, _ in segment}
        spans = {name: observations[end] - observations[start] for name, start, end in STAGES}
        spans["child_pid"] = events[ending][2]
        spans["launch_start_ns"] = stamp
        records.append(spans)
    if len(records) != 17:
        raise AssertionError(f"expected 17 launch records, got {len(records)}")
    return records


def main() -> None:
    selected = []
    with OUTPUT.open("w") as output:
        for line_number, line in enumerate(RAW.read_text().splitlines(), 1):
            command = json.loads(line)
            if command["phase"] != "traced":
                continue
            events = parse_events(command["stderr"])
            launches = launch_records(events)
            row = {
                "round": command["round"],
                "raw_source": f"{RAW}:{line_number}",
                "binary_status": command["status"],
                "selected": command["selected"],
                "passed": command["passed"],
                "event_count": len(events),
                "launches": launches,
                "stage_sum_ns": {name: sum(child[name] for child in launches) for name, _, _ in STAGES},
                "stage_max_ns": {name: max(child[name] for child in launches) for name, _, _ in STAGES},
            }
            output.write(json.dumps(row) + "\n")
            selected.append(row)
    if len(selected) != 5:
        raise AssertionError(f"expected 5 traced commands, got {len(selected)}")
    print(json.dumps({
        "traced_commands": len(selected),
        "exact_children_per_command": 17,
        "median_stage_sum_ns": {name: statistics.median(row["stage_sum_ns"][name] for row in selected) for name, _, _ in STAGES},
        "largest_single_posix_spawn_ns": max(row["stage_max_ns"]["posix_spawn_ns"] for row in selected),
    }))


if __name__ == "__main__":
    main()
