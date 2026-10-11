#!/usr/bin/env python3
"""Observe test process overlap separately from wall-time measurement."""

import json
import os
from pathlib import Path
import subprocess
import time

from measure_c54b_macos import ROOT


EVIDENCE = ROOT / "specs/cargo-val/evidence/c54b/workload_10m_pair20"
FIXTURE = Path("/tmp/cargo_val_fixture_10m/Cargo.toml")
TARGET = Path("/tmp/cargo_val_workload10_target")
BINARY = Path("/tmp/cargo_val_c54b_candidate/cargo-val")
PREFIX = str(TARGET / "debug/deps/")


def snapshot() -> list[dict]:
    result = subprocess.run(["ps", "-axo", "pid,ppid,command"], capture_output=True, text=True, check=True)
    records = []
    for line in result.stdout.splitlines()[1:]:
        if PREFIX not in line:
            continue
        fields = line.strip().split(maxsplit=2)
        if len(fields) != 3:
            continue
        process_id, parent_id, command = fields
        if not command.startswith(PREFIX):
            continue
        records.append({
            "pid": int(process_id), "ppid": int(parent_id), "command": command,
            "kind": "listing" if " --list" in command else "exact" if " --exact" in command else "binary",
        })
    return records


def main() -> None:
    environment = os.environ.copy()
    environment.update(CARGO_TARGET_DIR=str(TARGET), CARGO_VAL_JOBS="17", RUST_TEST_THREADS="10", NO_COLOR="1")
    environment["PATH"] = str(BINARY.parent) + os.pathsep + environment.get("PATH", "")
    args = ["--manifest-path", str(FIXTURE), "--tests", "--jobs", "10"]
    commands = {
        "cargo_test": ["cargo", "test", *args, "--", "--test-threads", "10"],
        "cargo_val": ["cargo", "val", *args],
        "nextest": ["cargo", "nextest", "run", "--manifest-path", str(FIXTURE), "-j", "10"],
    }
    with (EVIDENCE / "process-overlap.raw.jsonl").open("w") as output:
        for arm, command in commands.items():
            started = time.monotonic_ns()
            child = subprocess.Popen(command, cwd=ROOT, env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            samples = []
            while child.poll() is None:
                samples.append({"stamp_ns": time.monotonic_ns(), "processes": snapshot()})
                time.sleep(0.005)
            stdout, stderr = child.communicate()
            record = {
                "arm": arm, "command": command, "status": child.returncode,
                "started_ns": started, "completed_ns": time.monotonic_ns(),
                "sample_count": len(samples), "peak_binary_processes": max((len(sample["processes"]) for sample in samples), default=0),
                "peak_exact_processes": max((sum(process["kind"] == "exact" for process in sample["processes"]) for sample in samples), default=0),
                "distinct_observed_pids": len({process["pid"] for sample in samples for process in sample["processes"]}),
                "snapshots": samples,
                "stdout": stdout.decode("utf-8", "replace"), "stderr": stderr.decode("utf-8", "replace"),
            }
            output.write(json.dumps(record) + "\n")
            output.flush()
            if record["status"] != 0:
                raise AssertionError(f"{arm} process probe failed")
            print(json.dumps({key: record[key] for key in ["arm", "status", "sample_count", "peak_binary_processes", "peak_exact_processes", "distinct_observed_pids"]}))


if __name__ == "__main__":
    main()
