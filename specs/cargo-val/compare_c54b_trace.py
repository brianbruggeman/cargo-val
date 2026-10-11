#!/usr/bin/env python3
"""Compare an isolated phase-instrumented runner with the accepted runner."""

import hashlib
import json
import os
from pathlib import Path
import platform
import random
import statistics

from measure_c54b_macos import FIXTURE, ROOT, expected_names, invoke


EVIDENCE = ROOT / "specs/cargo-val/evidence/c54b/trace_pair"
TARGET = Path("/tmp/cargo_val_slot_target")
BINARIES = {
    "plain": Path("/tmp/cargo_val_c54b_candidate/cargo-val"),
    "trace": Path("/tmp/cargo_val_c54b_trace_bin/cargo-val"),
}
ROUNDS = 5
SEED = 540019


def events(stderr: str) -> list[dict]:
    parsed = []
    for line in stderr.splitlines():
        if line.startswith("C54TRACE "):
            _, kind, stamp, value = line.split()
            parsed.append({"kind": kind, "stamp_ns": int(stamp), "value": int(value)})
    return parsed


def main() -> None:
    if platform.system() != "Darwin":
        raise AssertionError("macOS phase trace")
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment.update(CARGO_TARGET_DIR=str(TARGET), CARGO_VAL_JOBS="17", RUST_TEST_THREADS="10", NO_COLOR="1")
    command = ["cargo", "val", "--manifest-path", str(FIXTURE), "--tests", "--jobs", "10"]
    conditions = {
        "target_dir": str(TARGET), "fixture": str(FIXTURE), "command": command,
        "jobs": 17, "cargo_build_jobs": 10, "test_threads": 10,
        "binary_sha256": {arm: hashlib.sha256(path.read_bytes()).hexdigest() for arm, path in BINARIES.items()},
        "trace_source_sha256": hashlib.sha256(Path("/tmp/cargo_val_c54b_trace_src/src/host/unix_driver.rs").read_bytes()).hexdigest(),
        "trace_injection_script": "/tmp/cargo_val_c54b_trace_inject.py",
        "round_order_seed": SEED, "warmup_rounds": 1, "paired_rounds": ROUNDS,
        "capture": "same normal cargo val command, target, fixture, jobs, and /usr/bin/time wrapper; PATH selects binary",
    }
    (EVIDENCE / "conditions.json").write_text(json.dumps(conditions, indent=2) + "\n")
    expected = expected_names()
    paired = []
    with (EVIDENCE / "paired-trace.raw.jsonl").open("w") as output:
        for round_index in range(ROUNDS + 1):
            arms = list(BINARIES)
            random.Random(SEED + round_index).shuffle(arms)
            for arm in arms:
                selected_env = environment.copy()
                selected_env["PATH"] = str(BINARIES[arm].parent) + os.pathsep + selected_env.get("PATH", "")
                record = invoke(command, selected_env, "cargo_val", round_index)
                record.update(arm=arm, phase="warmup" if round_index == 0 else "paired", trace_events=events(record["stderr"]))
                output.write(json.dumps(record) + "\n")
                output.flush()
                if record["status"] != 0 or set(record["test_results"]) != expected or record["passed"] != 17:
                    raise AssertionError(f"{arm} round {round_index} incomplete")
                if arm == "trace" and not record["trace_events"]:
                    raise AssertionError("trace arm emitted no phase records")
                if round_index:
                    paired.append(record)
    summary = {"conditions": conditions, "arms": {}, "within_round_trace_minus_plain_ns": []}
    for arm in BINARIES:
        values = [record["elapsed_ns"] for record in paired if record["arm"] == arm]
        summary["arms"][arm] = {"elapsed_ns": values, "median_elapsed_ns": statistics.median(values), "coefficient_of_variation": statistics.pstdev(values) / statistics.mean(values)}
    for round_index in range(1, ROUNDS + 1):
        current = {record["arm"]: record for record in paired if record["round"] == round_index}
        summary["within_round_trace_minus_plain_ns"].append(current["trace"]["elapsed_ns"] - current["plain"]["elapsed_ns"])
    (EVIDENCE / "paired-trace.summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({"paired_rounds": ROUNDS, "selected_per_run": 17, "median_elapsed_ns": {arm: data["median_elapsed_ns"] for arm, data in summary["arms"].items()}, "paired_trace_minus_plain_ns": summary["within_round_trace_minus_plain_ns"]}))


if __name__ == "__main__":
    main()
