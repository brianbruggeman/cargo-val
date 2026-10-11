#!/usr/bin/env python3
"""Compare three normal Cargo commands on an isolated 10M-iteration fixture."""

import hashlib
import json
import os
from pathlib import Path
import platform
import random
import statistics

from measure_c54b_macos import ROOT, expected_names, invoke


EVIDENCE = Path(os.environ.get("C54_10M_EVIDENCE", str(ROOT / "specs/cargo-val/evidence/c54b/workload_10m")))
FIXTURE_ROOT = Path("/tmp/cargo_val_fixture_10m")
FIXTURE = FIXTURE_ROOT / "Cargo.toml"
TARGET = Path("/tmp/cargo_val_workload10_target")
BINARY = Path("/tmp/cargo_val_c54b_candidate/cargo-val")
ROUNDS = int(os.environ.get("C54_10M_ROUNDS", "5"))
SEED = 540010


def main() -> None:
    if platform.system() != "Darwin":
        raise AssertionError("macOS workload comparison")
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    expected = expected_names()
    environment = os.environ.copy()
    environment.update(CARGO_TARGET_DIR=str(TARGET), CARGO_VAL_JOBS="17", RUST_TEST_THREADS="10", NO_COLOR="1")
    environment["PATH"] = str(BINARY.parent) + os.pathsep + environment.get("PATH", "")
    args = ["--manifest-path", str(FIXTURE), "--tests", "--jobs", "10"]
    commands = {
        "cargo_test": ["cargo", "test", *args, "--", "--test-threads", "10"],
        "cargo_val": ["cargo", "val", *args],
        "nextest": ["cargo", "nextest", "run", "--manifest-path", str(FIXTURE), "-j", "10"],
    }
    source_hashes = {
        source.name: hashlib.sha256(source.read_bytes()).hexdigest()
        for source in sorted((FIXTURE_ROOT / "tests").glob("*_fixture.rs"))
    }
    conditions = {
        "host": platform.platform(), "fixture": str(FIXTURE), "target_dir": str(TARGET),
        "fixture_source_sha256": source_hashes,
        "fixture_diff": "specs/cargo-val/evidence/c54b/workload-10m-fixture.diff",
        "runner_binary_sha256": hashlib.sha256(BINARY.read_bytes()).hexdigest(),
        "commands": commands, "cargo_val_jobs": 17, "cargo_build_jobs": 10,
        "cargo_test_threads": 10, "nextest_jobs": 10,
        "warmup_rounds": 1, "paired_rounds": ROUNDS, "round_order_seed": SEED,
        "capture": "same debug test artifacts and /usr/bin/time wrapper for each arm",
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
                if record["status"] != 0 or set(record["test_results"]) != expected or record["passed"] != 17:
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
        "paired_rounds": ROUNDS, "selected_per_run": 17,
        "median_elapsed_ns": {arm: value["median_elapsed_ns"] for arm, value in summary["arms"].items()},
        "val_faster_than_cargo_test_pairs": sum(delta < 0 for delta in summary["within_round_val_minus_cargo_test_ns"]),
    }))


if __name__ == "__main__":
    main()
