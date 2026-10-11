#!/usr/bin/env python3
"""Capture paired warm macOS command runs on the 17-test fixture."""

import hashlib
import json
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import time


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "tests/fixtures/workspace/Cargo.toml"
EVIDENCE = Path(os.environ.get("C54_EVIDENCE", str(ROOT / "specs/cargo-val/evidence/c54b")))
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", "/tmp/cargo_target"))
VAL_BINARY = Path(os.environ.get("CARGO_VAL_BINARY", str(TARGET / "release/cargo-val")))
RUN_COUNT = 5
JOBS = int(os.environ.get("C54_JOBS", "4"))
VAL_DEFAULT_JOBS = os.environ.get("C54_VAL_JOBS_UNSET") == "1"
VAL_DIRECT = os.environ.get("C54_VAL_DIRECT") == "1"
TIME_PATTERN = re.compile(r"^\s*(\d+)\s+(maximum resident set size|peak memory footprint)$", re.M)
CPU_PATTERN = re.compile(r"^\s*(\d+\.\d+) real\s+(\d+\.\d+) user\s+(\d+\.\d+) sys$", re.M)
CARGO_TEST_PATTERN = re.compile(r"^test (.+) \.\.\. (ok|FAILED|ignored)$", re.M)
VAL_PATTERN = re.compile(r"^(.+) (PASS|FAIL)$", re.M)
NEXTEST_PATTERN = re.compile(r"^\s*(PASS|FAIL|SKIP) \[\s*([\d.]+)s\] \(\s*\d+/17\) \S+ (.+)$", re.M)


def invoke(command: list[str], environment: dict[str, str], runner: str, round_index: int) -> dict:
    started_ns = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
    try:
        completed = subprocess.run(
            ["/usr/bin/time", "-l", *command],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            timeout=180,
            check=False,
        )
        status = completed.returncode
        stdout = completed.stdout.decode("utf-8", "replace")
        stderr = completed.stderr.decode("utf-8", "replace")
    except subprocess.TimeoutExpired as error:
        status = "timeout"
        stdout = (error.stdout or b"").decode("utf-8", "replace")
        stderr = (error.stderr or b"").decode("utf-8", "replace")
    completed_ns = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
    elapsed_ns = completed_ns - started_ns
    usage = {name.replace(" ", "_"): int(value) for value, name in TIME_PATTERN.findall(stderr)}
    cpu = CPU_PATTERN.search(stderr)
    if runner == "cargo_test":
        results = {name: result for name, result in CARGO_TEST_PATTERN.findall(stdout)}
        passed = sum(result == "ok" for result in results.values())
        failed = sum(result == "FAILED" for result in results.values())
    elif runner == "cargo_val":
        results = {name: result for name, result in VAL_PATTERN.findall(stdout)}
        passed = sum(result == "PASS" for result in results.values())
        failed = sum(result == "FAIL" for result in results.values())
    else:
        matches = NEXTEST_PATTERN.findall(stderr)
        results = {name: result for result, _, name in matches}
        passed = sum(result == "PASS" for result in results.values())
        failed = sum(result == "FAIL" for result in results.values())
    record = {
        "runner": runner,
        "round": round_index,
        "command": command,
        "elapsed_ns": elapsed_ns,
        "started_monotonic_ns": started_ns,
        "completed_monotonic_ns": completed_ns,
        "status": status,
        "selected": len(results),
        "passed": passed,
        "failed": failed,
        "test_results": results,
        "per_test_latency_seconds": (
            {name: float(seconds) for _, seconds, name in matches} if runner == "nextest" else None
        ),
        "command_process_user_seconds": float(cpu.group(2)) if cpu else None,
        "command_process_system_seconds": float(cpu.group(3)) if cpu else None,
        "maximum_resident_set_size_bytes": usage.get("maximum_resident_set_size"),
        "peak_memory_footprint_bytes": usage.get("peak_memory_footprint"),
        "stdout": stdout,
        "stderr": stderr,
    }
    return record


def expected_names() -> set[str]:
    fixture_tests = ROOT / "tests/fixtures/workspace/tests"
    names = set()
    for source in sorted(fixture_tests.glob("*_fixture.rs")):
        content = source.read_text()
        found = re.findall(r"#\[test\]\s*fn\s+(\w+)", content)
        for name in found:
            names.add(f"fixture::{name}" if source.name == "integration_fixture.rs" else name)
    if len(names) != 17:
        raise AssertionError(f"fixture has {len(names)} unique test names, expected 17")
    return names


def main() -> None:
    if platform.system() != "Darwin":
        raise AssertionError("C54b is the macOS measurement")
    if not VAL_BINARY.is_file():
        raise AssertionError(f"release cargo-val binary missing: {VAL_BINARY}")
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(TARGET)
    if VAL_DEFAULT_JOBS:
        environment.pop("CARGO_VAL_JOBS", None)
    else:
        environment["CARGO_VAL_JOBS"] = str(JOBS)
    environment["RUST_TEST_THREADS"] = str(JOBS)
    environment["NO_COLOR"] = "1"
    environment["PATH"] = str(VAL_BINARY.parent) + os.pathsep + environment.get("PATH", "")
    commands = {
        "cargo_test": ["cargo", "test", "--manifest-path", str(FIXTURE), "--tests", "--jobs", str(JOBS), "--", "--test-threads", str(JOBS)],
        "cargo_val": ([str(VAL_BINARY), "val"] if VAL_DIRECT else ["cargo", "val"]) + ["--manifest-path", str(FIXTURE), "--tests", "--jobs", str(JOBS)],
        "nextest": ["cargo", "nextest", "run", "--manifest-path", str(FIXTURE), "-j", str(JOBS)],
    }
    conditions = {
        "host": platform.platform(),
        "fixture": str(FIXTURE),
        "target_dir": str(TARGET),
        "jobs": JOBS,
        "cargo_val_jobs_environment": None if VAL_DEFAULT_JOBS else JOBS,
        "cargo_val_direct_binary": VAL_DIRECT,
        "runner_binary": str(VAL_BINARY),
        "runner_binary_sha256": hashlib.sha256(VAL_BINARY.read_bytes()).hexdigest(),
        "commands": commands,
        "capture": "subprocess pipes; wall clock wraps /usr/bin/time -l and child exit",
        "cpu_rss_scope": "/usr/bin/time -l reports command process usage; child-test-process totals are unmeasured",
        "per_test_latency_scope": "nextest emits per-test durations; cargo test and cargo val do not emit them",
        "warmup_rounds": 1,
        "paired_rounds": RUN_COUNT,
    }
    (EVIDENCE / "conditions.json").write_text(json.dumps(conditions, indent=2) + "\n")
    expected = expected_names()
    all_records = []
    with (EVIDENCE / "paired-runs.raw.jsonl").open("w") as output:
        for round_index in range(RUN_COUNT + 1):
            order = list(commands)
            order = order[round_index % len(order):] + order[:round_index % len(order)]
            for runner in order:
                record = invoke(commands[runner], environment, runner, round_index)
                record["phase"] = "warmup" if round_index == 0 else "paired"
                output.write(json.dumps(record) + "\n")
                output.flush()
                if record["status"] != 0 or set(record["test_results"]) != expected:
                    raise AssertionError(f"{runner} round {round_index} returned {record['status']} and {record['selected']} names; see paired-runs.raw.jsonl")
                if record["passed"] != 17 or record["failed"] != 0:
                    raise AssertionError(f"{runner} round {round_index} pass/fail counts differ; see paired-runs.raw.jsonl")
                if round_index:
                    all_records.append(record)
    summary = {"conditions": conditions, "runners": {}}
    for runner in commands:
        samples = [record["elapsed_ns"] for record in all_records if record["runner"] == runner]
        summary["runners"][runner] = {
            "elapsed_ns": samples,
            "median_elapsed_ns": statistics.median(samples),
            "coefficient_of_variation": statistics.pstdev(samples) / statistics.mean(samples),
            "throughput_tests_per_second": [17_000_000_000 / value for value in samples],
            "status_counts": {"zero": 5, "nonzero": 0},
        }
    reference = summary["runners"]["cargo_test"]["median_elapsed_ns"]
    nextest = summary["runners"]["nextest"]["median_elapsed_ns"]
    current = summary["runners"]["cargo_val"]["median_elapsed_ns"]
    summary["cargo_val_fraction_of_cargo_test"] = current / reference
    summary["cargo_val_fraction_of_nextest"] = current / nextest
    (EVIDENCE / "paired-runs.summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({"paired_runs_per_runner": RUN_COUNT, "selected_per_run": 17, "median_elapsed_ns": {runner: row["median_elapsed_ns"] for runner, row in summary["runners"].items()}, "cargo_val_fraction_of_cargo_test": summary["cargo_val_fraction_of_cargo_test"], "cargo_val_fraction_of_nextest": summary["cargo_val_fraction_of_nextest"]}))


if __name__ == "__main__":
    main()
