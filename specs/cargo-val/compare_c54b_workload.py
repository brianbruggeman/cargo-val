#!/usr/bin/env python3
"""Paired filtered workload control on the same fixture and target artifacts."""

import hashlib
import json
import os
from pathlib import Path
import platform
import random
import statistics

from measure_c54b_macos import FIXTURE, ROOT, expected_names, invoke


EVIDENCE = ROOT / "specs/cargo-val/evidence/c54b/workload_four"
TARGET = Path("/tmp/cargo_val_slot_target")
BINARY = Path("/tmp/cargo_val_c54b_candidate/cargo-val")
FILTER = "order_seed"
ROUNDS = 20
SEED = 540004


def main() -> None:
    if platform.system() != "Darwin":
        raise AssertionError("macOS workload comparison")
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    expected = {name for name in expected_names() if FILTER in name}
    if len(expected) != 4:
        raise AssertionError("expected four filtered tests")
    environment = os.environ.copy()
    environment.update(CARGO_TARGET_DIR=str(TARGET), CARGO_VAL_JOBS="17", RUST_TEST_THREADS="10", NO_COLOR="1")
    environment["PATH"] = str(BINARY.parent) + os.pathsep + environment.get("PATH", "")
    args = [FILTER, "--manifest-path", str(FIXTURE), "--tests", "--jobs", "10"]
    commands = {
        "cargo_test": ["cargo", "test", *args, "--", "--test-threads", "10"],
        "cargo_val": ["cargo", "val", *args],
    }
    conditions = {
        "fixture": str(FIXTURE), "filter": FILTER, "expected_names": sorted(expected),
        "target": str(TARGET), "binary_sha256": hashlib.sha256(BINARY.read_bytes()).hexdigest(),
        "commands": commands, "cargo_val_jobs": 17, "cargo_build_jobs": 10,
        "cargo_test_threads": 10, "warmup_rounds": 1, "paired_rounds": ROUNDS,
        "round_order_seed": SEED, "capture": "same debug test artifacts and /usr/bin/time wrapper as 17-test batch",
    }
    (EVIDENCE / "conditions.json").write_text(json.dumps(conditions, indent=2) + "\n")
    paired = []
    with (EVIDENCE / "paired-workload.raw.jsonl").open("w") as output:
        for round_index in range(ROUNDS + 1):
            arms = list(commands)
            random.Random(SEED + round_index).shuffle(arms)
            for arm in arms:
                record = invoke(commands[arm], environment, arm, round_index)
                record.update(arm=arm, phase="warmup" if round_index == 0 else "paired")
                output.write(json.dumps(record) + "\n")
                output.flush()
                if record["status"] != 0 or set(record["test_results"]) != expected or record["passed"] != 4:
                    raise AssertionError(f"{arm} round {round_index} incomplete")
                if round_index:
                    paired.append(record)
    summary = {"conditions": conditions, "arms": {}, "within_round_val_minus_cargo_test_ns": []}
    for arm in commands:
        samples = [record["elapsed_ns"] for record in paired if record["arm"] == arm]
        summary["arms"][arm] = {
            "elapsed_ns": samples,
            "median_elapsed_ns": statistics.median(samples),
            "coefficient_of_variation": statistics.pstdev(samples) / statistics.mean(samples),
        }
    for round_index in range(1, ROUNDS + 1):
        current = {record["arm"]: record for record in paired if record["round"] == round_index}
        summary["within_round_val_minus_cargo_test_ns"].append(current["cargo_val"]["elapsed_ns"] - current["cargo_test"]["elapsed_ns"])
    (EVIDENCE / "paired-workload.summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({
        "paired_rounds": ROUNDS, "selected_per_run": 4,
        "median_elapsed_ns": {arm: value["median_elapsed_ns"] for arm, value in summary["arms"].items()},
        "val_faster_pairs": sum(delta < 0 for delta in summary["within_round_val_minus_cargo_test_ns"]),
    }))


if __name__ == "__main__":
    main()
