#!/usr/bin/env python3
"""Paired normal Cargo commands with one test-process slot-cap toggle."""

import hashlib
import json
import os
from pathlib import Path
import platform
import random
import statistics

from measure_c54b_macos import FIXTURE, ROOT, expected_names, invoke


EVIDENCE = Path(os.environ.get("C54_SLOTS_EVIDENCE", str(ROOT / "specs/cargo-val/evidence/c54b/slot_pair17")))
BINARY = Path("/tmp/cargo_val_c54b_candidate/cargo-val")
TARGET = Path(os.environ.get("C54_SLOTS_TARGET", "/tmp/cargo_target"))
ROUNDS = 20
SEED = 541017
JOBS = tuple(int(value) for value in os.environ.get("C54_SLOTS_JOBS", "10,17").split(","))


def main() -> None:
    if platform.system() != "Darwin":
        raise AssertionError("C54b is the macOS comparison")
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment.update(CARGO_TARGET_DIR=str(TARGET), RUST_TEST_THREADS="10", NO_COLOR="1")
    environment["PATH"] = str(BINARY.parent) + os.pathsep + environment.get("PATH", "")
    args = ["--manifest-path", str(FIXTURE), "--tests", "--jobs", "10"]
    commands = {"cargo_test": ["cargo", "test", *args, "--", "--test-threads", "10"]}
    commands.update({f"val_{jobs}": ["cargo", "val", *args] for jobs in JOBS})
    conditions = {
        "host": platform.platform(),
        "fixture": str(FIXTURE),
        "target_dir": str(TARGET),
        "binary_sha256": hashlib.sha256(BINARY.read_bytes()).hexdigest(),
        "commands": commands,
        "cargo_val_jobs_by_arm": {f"val_{jobs}": jobs for jobs in JOBS},
        "cargo_build_jobs": 10,
        "cargo_test_threads": 10,
        "round_order_seed": SEED,
        "warmup_rounds": 1,
        "paired_rounds": ROUNDS,
        "capture": "normal cargo val via the same release binary; arm changes CARGO_VAL_JOBS only",
    }
    (EVIDENCE / "conditions.json").write_text(json.dumps(conditions, indent=2) + "\n")
    expected = expected_names()
    paired = []
    with (EVIDENCE / "paired-slots.raw.jsonl").open("w") as output:
        for round_index in range(ROUNDS + 1):
            order = list(commands)
            random.Random(SEED + round_index).shuffle(order)
            for arm in order:
                arm_environment = environment.copy()
                if arm != "cargo_test":
                    arm_environment["CARGO_VAL_JOBS"] = arm.removeprefix("val_")
                else:
                    arm_environment.pop("CARGO_VAL_JOBS", None)
                runner = "cargo_test" if arm == "cargo_test" else "cargo_val"
                record = invoke(commands[arm], arm_environment, runner, round_index)
                record.update(arm=arm, phase="warmup" if round_index == 0 else "paired")
                output.write(json.dumps(record) + "\n")
                output.flush()
                if record["status"] != 0 or set(record["test_results"]) != expected or record["passed"] != 17:
                    raise AssertionError(f"{arm} round {round_index} was incomplete; see raw output")
                if round_index:
                    paired.append(record)
    summary = {"conditions": conditions, "arms": {}, "within_round_deltas_ns": {}}
    for arm in commands:
        samples = [record["elapsed_ns"] for record in paired if record["arm"] == arm]
        summary["arms"][arm] = {
            "elapsed_ns": samples,
            "median_elapsed_ns": statistics.median(samples),
            "coefficient_of_variation": statistics.pstdev(samples) / statistics.mean(samples),
            "selected_per_run": 17,
            "passed_per_run": 17,
        }
    for jobs in JOBS:
        arm = f"val_{jobs}"
        summary["within_round_deltas_ns"][f"{arm}_minus_cargo_test"] = []
        for round_index in range(1, ROUNDS + 1):
            current = {record["arm"]: record for record in paired if record["round"] == round_index}
            summary["within_round_deltas_ns"][f"{arm}_minus_cargo_test"].append(
                current[arm]["elapsed_ns"] - current["cargo_test"]["elapsed_ns"]
            )
    (EVIDENCE / "paired-slots.summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({
        "paired_rounds": ROUNDS,
        "selected_per_run": 17,
        "median_elapsed_ns": {arm: row["median_elapsed_ns"] for arm, row in summary["arms"].items()},
        "faster_than_cargo_test_pairs": {
            arm: sum(delta < 0 for delta in summary["within_round_deltas_ns"][f"{arm}_minus_cargo_test"])
            for arm in commands if arm != "cargo_test"
        },
    }))


if __name__ == "__main__":
    main()
