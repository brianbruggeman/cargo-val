#!/usr/bin/env python3
"""Randomized paired normal Cargo commands for one-listing lookahead."""

import hashlib
import json
import os
from pathlib import Path
import platform
import random
import statistics

from measure_c54b_macos import FIXTURE, ROOT, expected_names, invoke


EVIDENCE = Path(os.environ.get("C54_LOOKAHEAD_EVIDENCE", str(ROOT / "specs/cargo-val/evidence/c54b/lookahead_pair")))
CONTROL_BINARY = Path("/tmp/cargo_val_c54b_control/cargo-val")
CANDIDATE_BINARY = Path("/tmp/cargo_val_c54b_candidate/cargo-val")
TARGET = Path("/tmp/cargo_target")
JOBS = 10
ROUNDS = int(os.environ.get("C54_LOOKAHEAD_ROUNDS", "5"))
SEED = 541010


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main() -> None:
    if platform.system() != "Darwin":
        raise AssertionError("the C54b lookahead comparison is macOS only")
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    base_environment = os.environ.copy()
    base_environment.update(CARGO_TARGET_DIR=str(TARGET), RUST_TEST_THREADS=str(JOBS), NO_COLOR="1")
    base_environment.pop("CARGO_VAL_JOBS", None)
    args = ["--manifest-path", str(FIXTURE), "--tests", "--jobs", str(JOBS)]
    commands = {
        "cargo_test": ["cargo", "test", *args, "--", "--test-threads", str(JOBS)],
        "control": ["cargo", "val", *args],
        "lookahead": ["cargo", "val", *args],
    }
    binaries = {"control": CONTROL_BINARY, "lookahead": CANDIDATE_BINARY}
    conditions = {
        "host": platform.platform(),
        "fixture": str(FIXTURE),
        "target_dir": str(TARGET),
        "jobs": JOBS,
        "cargo_val_jobs_environment": None,
        "commands": commands,
        "binary_sha256": {arm: sha256(binary) for arm, binary in binaries.items()},
        "control_source": "/tmp/cargo_val_c54b_control_src/src/host/unix_driver.rs",
        "source_diff": "evidence/c54b/lookahead-control-to-candidate.diff",
        "warmup_rounds": 1,
        "paired_rounds": ROUNDS,
        "round_order_seed": SEED,
        "capture": "normal cargo val via arm-specific PATH; same /usr/bin/time -l wrapper and child target dir",
    }
    (EVIDENCE / "conditions.json").write_text(json.dumps(conditions, indent=2) + "\n")
    expected = expected_names()
    paired = []
    with (EVIDENCE / "paired-lookahead.raw.jsonl").open("w") as output:
        for round_index in range(ROUNDS + 1):
            order = list(commands)
            random.Random(SEED + round_index).shuffle(order)
            for arm in order:
                environment = base_environment.copy()
                binary = binaries.get(arm, CANDIDATE_BINARY)
                environment["PATH"] = str(binary.parent) + os.pathsep + environment.get("PATH", "")
                runner = "cargo_test" if arm == "cargo_test" else "cargo_val"
                record = invoke(commands[arm], environment, runner, round_index)
                record.update(arm=arm, phase="warmup" if round_index == 0 else "paired")
                output.write(json.dumps(record) + "\n")
                output.flush()
                if record["status"] != 0 or set(record["test_results"]) != expected or record["passed"] != 17:
                    raise AssertionError(f"{arm} round {round_index} was incomplete; see raw output")
                if round_index:
                    paired.append(record)
    summary = {"conditions": conditions, "arms": {}, "within_round_lookahead_minus_control_ns": []}
    for arm in commands:
        elapsed = [record["elapsed_ns"] for record in paired if record["arm"] == arm]
        summary["arms"][arm] = {
            "elapsed_ns": elapsed,
            "median_elapsed_ns": statistics.median(elapsed),
            "coefficient_of_variation": statistics.pstdev(elapsed) / statistics.mean(elapsed),
            "selected_per_run": 17,
            "passed_per_run": 17,
        }
    for round_index in range(1, ROUNDS + 1):
        control = next(record for record in paired if record["arm"] == "control" and record["round"] == round_index)
        lookahead = next(record for record in paired if record["arm"] == "lookahead" and record["round"] == round_index)
        summary["within_round_lookahead_minus_control_ns"].append(lookahead["elapsed_ns"] - control["elapsed_ns"])
    (EVIDENCE / "paired-lookahead.summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({
        "paired_rounds": ROUNDS,
        "selected_per_run": 17,
        "median_elapsed_ns": {arm: row["median_elapsed_ns"] for arm, row in summary["arms"].items()},
        "within_round_lookahead_minus_control_ns": summary["within_round_lookahead_minus_control_ns"],
    }))


if __name__ == "__main__":
    main()
