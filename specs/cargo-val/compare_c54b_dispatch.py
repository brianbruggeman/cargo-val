#!/usr/bin/env python3
"""Pair Cargo test, Cargo external dispatch, and the same cargo-val binary."""

import hashlib
import json
import os
from pathlib import Path
import platform
import random
import statistics

from measure_c54b_macos import FIXTURE, ROOT, invoke, expected_names


TARGET = Path(os.environ.get("CARGO_TARGET_DIR", "/tmp/cargo_target"))
VAL_BINARY = Path(os.environ.get("CARGO_VAL_BINARY", str(TARGET / "release/cargo-val")))
EVIDENCE = Path(os.environ.get("C54_DISPATCH_EVIDENCE", str(ROOT / "specs/cargo-val/evidence/c54b/dispatch_pair")))
JOBS = 10
SEED = 521010


def phase_time(stderr: str, label: str) -> int | None:
    stamps = [int(parts[2]) for line in stderr.splitlines() if (parts := line.split())[:2] == ["C54PHASE", label]]
    return stamps[0] if len(stamps) == 1 else None


def main() -> None:
    if platform.system() != "Darwin":
        raise AssertionError("C54b dispatch comparison runs on macOS")
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(TARGET)
    environment["CARGO_VAL_JOBS"] = str(JOBS)
    environment["RUST_TEST_THREADS"] = str(JOBS)
    environment["NO_COLOR"] = "1"
    environment["PATH"] = str(VAL_BINARY.parent) + os.pathsep + environment.get("PATH", "")
    args = ["--manifest-path", str(FIXTURE), "--tests", "--jobs", str(JOBS)]
    commands = {
        "cargo_test": ["cargo", "test", *args, "--", "--test-threads", str(JOBS)],
        "cargo_val": ["cargo", "val", *args],
        "direct_val": [str(VAL_BINARY), "val", *args],
    }
    conditions = {
        "host": platform.platform(),
        "fixture": str(FIXTURE),
        "target_dir": str(TARGET),
        "jobs": JOBS,
        "binary": str(VAL_BINARY),
        "binary_sha256": hashlib.sha256(VAL_BINARY.read_bytes()).hexdigest(),
        "commands": commands,
        "effective_val_argv_after_cargo_dispatch": commands["direct_val"][1:],
        "round_order_seed": SEED,
        "warmup_rounds": 1,
        "paired_rounds": 5,
        "capture": "subprocess pipes; identical /usr/bin/time -l wrapper and target artifacts",
    }
    (EVIDENCE / "conditions.json").write_text(json.dumps(conditions, indent=2) + "\n")
    expected = expected_names()
    records = []
    with (EVIDENCE / "paired-dispatch.raw.jsonl").open("w") as output:
        for round_index in range(6):
            order = list(commands)
            random.Random(SEED + round_index).shuffle(order)
            for arm in order:
                record = invoke(commands[arm], environment, "cargo_test" if arm == "cargo_test" else "cargo_val", round_index)
                record["arm"] = arm
                record["phase"] = "warmup" if round_index == 0 else "paired"
                start = phase_time(record["stderr"], "run_start")
                done = phase_time(record["stderr"], "run_done")
                record["binary_run_start_ns"] = start
                record["binary_run_done_ns"] = done
                record["before_binary_run_ns"] = start - record["started_monotonic_ns"] if start is not None else None
                record["after_binary_run_ns"] = record["completed_monotonic_ns"] - done if done is not None else None
                output.write(json.dumps(record) + "\n")
                output.flush()
                if record["status"] != 0 or set(record["test_results"]) != expected or record["passed"] != 17:
                    raise AssertionError(f"{arm} round {round_index} differed; see paired-dispatch.raw.jsonl")
                if round_index:
                    records.append(record)
    summary = {"conditions": conditions, "arms": {}, "within_round_val_difference_ns": []}
    for arm in commands:
        selected = [record for record in records if record["arm"] == arm]
        summary["arms"][arm] = {
            "elapsed_ns": [record["elapsed_ns"] for record in selected],
            "median_elapsed_ns": statistics.median(record["elapsed_ns"] for record in selected),
            "before_binary_run_ns": [record["before_binary_run_ns"] for record in selected],
            "after_binary_run_ns": [record["after_binary_run_ns"] for record in selected],
            "median_before_binary_run_ns": statistics.median(record["before_binary_run_ns"] for record in selected) if all(record["before_binary_run_ns"] is not None for record in selected) else None,
        }
    for round_index in range(1, 6):
        normal = next(record for record in records if record["round"] == round_index and record["arm"] == "cargo_val")
        direct = next(record for record in records if record["round"] == round_index and record["arm"] == "direct_val")
        summary["within_round_val_difference_ns"].append(normal["elapsed_ns"] - direct["elapsed_ns"])
    (EVIDENCE / "paired-dispatch.summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({"paired_rounds": 5, "selected_per_run": 17, "median_elapsed_ns": {arm: data["median_elapsed_ns"] for arm, data in summary["arms"].items()}, "median_within_round_val_difference_ns": statistics.median(summary["within_round_val_difference_ns"]), "median_before_binary_run_ns": {arm: data["median_before_binary_run_ns"] for arm, data in summary["arms"].items()}}))


if __name__ == "__main__":
    main()
