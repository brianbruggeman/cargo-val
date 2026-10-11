import argparse
import json
import os
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
EVIDENCE = ROOT / "specs/cargo-val/evidence/c6"
REVISION = "6cffdc693bbd34483f1349f99b0ebd282c92fca4"
TEST_NAME = "holds_intent_until_completion_event"


def check_records(records, expected):
    if len(records) != 3:
        raise ValueError(f"expected 3 actual records, found {len(records)}")
    by_arm = {record["arm"]: record for record in records}
    if set(by_arm) != {"candidate", "proxima_direct", "zero_capacity"}:
        raise ValueError(f"unexpected arms: {sorted(by_arm)}")

    candidate = by_arm["candidate"]
    control = by_arm["proxima_direct"]
    zero_capacity = by_arm["zero_capacity"]
    intents = expected["intents"]
    for record in (candidate, control):
        if record["revision"] != REVISION:
            raise ValueError("Proxima revision mismatch")
        for key in (
            "names",
            "reads_before_deferred",
            "reads_while_deferred",
            "reads_after_retry",
            "available_while_deferred",
        ):
            if record[key] != intents[key]:
                raise ValueError(f"{record['arm']} {key} mismatch")
        if set(record["allocations"]) != {"first", "second", "deferred", "retry"}:
            raise ValueError(f"{record['arm']} missing allocation poll")
        if any(value < 0 for value in record["allocations"].values()):
            raise ValueError(f"{record['arm']} negative allocation delta")

    if candidate["allocations"] != control["allocations"]:
        raise ValueError("candidate added allocations to Proxima permit path")
    for key in ("source_reads", "available_permits"):
        if zero_capacity[key] != expected["zero_capacity"][key]:
            raise ValueError(f"zero-capacity {key} mismatch")
    if zero_capacity["positive_control_allocations"] != expected["positive_control"]["allocations"]:
        raise ValueError("allocation positive control mismatch")

    return {
        "candidate_records": 1,
        "proxima_control_records": 1,
        "zero_capacity_records": 1,
        "allocation_poll_deltas_per_arm": 4,
        "candidate_only_allocations": 0,
        "positive_control_allocations": zero_capacity["positive_control_allocations"],
        "observed_proxima_allocations": control["allocations"],
        "observed_zero_capacity_deferred_allocations": zero_capacity["deferred_allocations"],
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--expected", type=Path, required=True)
    arguments = parser.parse_args()

    expected_records = [
        json.loads(line) for line in arguments.expected.read_text().splitlines() if line
    ]
    expected = {record["kind"]: record for record in expected_records}
    if set(expected) != {"intents", "zero_capacity", "positive_control"}:
        raise ValueError("literal oracle must contain three expected records")

    command = [
        "cargo", "test", "-p", "cargo-val", "--no-default-features",
        "--features", "proxima-codec,proxima-primitives",
        "--test", "async_admission", TEST_NAME, "--", "--exact", "--nocapture",
        "--test-threads=1",
    ]
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = "/tmp/cargo_target"
    result = subprocess.run(command, cwd=ROOT, env=environment, capture_output=True, text=True)
    output = result.stdout + result.stderr
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    (EVIDENCE / "acceptance-cargo-test.txt").write_text(output)
    if result.returncode != 0:
        raise ValueError(f"cargo test exited {result.returncode}; see acceptance-cargo-test.txt")
    if "running 1 test" not in output or "1 passed; 0 failed" not in output:
        raise ValueError("exact test count was not one")

    records = [
        json.loads(line.split("C6_RECORD ", 1)[1])
        for line in output.splitlines()
        if "C6_RECORD " in line
    ]
    (EVIDENCE / "acceptance-records.jsonl").write_text(
        "".join(json.dumps(record, sort_keys=True) + "\n" for record in records)
    )
    summary = check_records(records, expected)

    runtime_sources = [
        ROOT / "Cargo.toml",
        *sorted((ROOT / "src").rglob("*.rs")),
        *sorted((ROOT / "tests").rglob("*.rs")),
    ]
    semaphore_references = [
        f"{path.relative_to(ROOT)}:{line_number}"
        for path in runtime_sources
        for line_number, line in enumerate(path.read_text().splitlines(), start=1)
        if "Semaphore" in line or "semaphore" in line
    ]
    if semaphore_references:
        raise ValueError(f"runner sources contain semaphore references: {semaphore_references}")
    summary["semaphore_references"] = 0

    mutated = [dict(record) for record in records]
    mutated[0] = dict(mutated[0], names=["suite::omega", "suite::beta", "suite::gamma"])
    try:
        check_records(mutated, expected)
    except ValueError:
        mutated_name_rejected = 1
    else:
        raise ValueError("mutated first name was accepted")
    try:
        check_records(records[:1] + records[2:], expected)
    except ValueError:
        missing_control_rejected = 1
    else:
        raise ValueError("missing Proxima control was accepted")
    summary["mutated_name_rejected"] = mutated_name_rejected
    summary["missing_control_rejected"] = missing_control_rejected
    (EVIDENCE / "acceptance-summary.txt").write_text(json.dumps(summary, sort_keys=True) + "\n")
    print(json.dumps(summary, sort_keys=True))


if __name__ == "__main__":
    main()
