import json
import os
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
EVIDENCE = ROOT / "specs/cargo-val/evidence/c7"
TARGET = "/tmp/cargo_target"
EXPECTED_TEST = "seven_two_slots"


def compare(records, expected):
    identities = [record for record in records if record["record"] == "identity"]
    summaries = [record for record in records if record["record"] == "summary"]
    names = [record["name"] for record in identities]
    if len(identities) != 7:
        raise ValueError(f"expected 7 identity records, found {len(identities)}")
    if names != expected["identities"]["names"]:
        raise ValueError(f"identity sequence mismatch: {names}")
    if any(record["admitted"] != 1 for record in identities):
        raise ValueError("an identity was not admitted exactly once")
    if len(set(names)) != 7:
        raise ValueError("duplicate identity detected")
    if len(summaries) != 1:
        raise ValueError(f"expected 1 summary record, found {len(summaries)}")
    summary = summaries[0]
    summary_values = {key: value for key, value in summary.items() if key != "record"}
    expected_summary = {key: value for key, value in expected["summary"].items() if key != "kind"}
    if summary_values != expected_summary:
        raise ValueError(f"summary mismatch: {summary_values}")
    if summary["capacity"] != expected["identities"]["capacity"]:
        raise ValueError("capacity mismatch")
    return {
        "identity_records": 7,
        "unique_identities": 7,
        "admitted_once": 7,
        "summary_records": 1,
        "peak_sessions": summary["peak_sessions"],
        "lost": summary["lost"],
    }


def run(command, label):
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = TARGET
    result = subprocess.run(command, cwd=ROOT, env=environment, capture_output=True, text=True)
    output = result.stdout + result.stderr
    (EVIDENCE / f"{label}.txt").write_text(output)
    if result.returncode:
        raise ValueError(f"{label} exited {result.returncode}; see evidence")
    return output


def main():
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    expected_records = [
        json.loads(line)
        for line in (ROOT / "specs/cargo-val/oracles/c7.jsonl").read_text().splitlines()
        if line
    ]
    expected = {record["kind"]: record for record in expected_records}
    if set(expected) != {"identities", "summary"}:
        raise ValueError("literal oracle must contain identities and summary")

    test_output = run(
        [
            "cargo", "test", "-p", "cargo-val", "--no-default-features",
            "--features", "proxima-codec,proxima-primitives",
            "--test", "async_admission", EXPECTED_TEST, "--", "--exact", "--nocapture",
            "--test-threads=1",
        ],
        "admission-test",
    )
    if "running 1 test" not in test_output or "1 passed; 0 failed" not in test_output:
        raise ValueError("expected exactly one passing admission test")
    records = []
    for line in test_output.splitlines():
        if "C7_IDENTITY " in line:
            row = json.loads(line.split("C7_IDENTITY ", 1)[1])
            records.append({"record": "identity", **row})
        elif "C7_SUMMARY " in line:
            row = json.loads(line.split("C7_SUMMARY ", 1)[1])
            records.append({"record": "summary", **row})
    (EVIDENCE / "admission-records.jsonl").write_text(
        "".join(json.dumps(record, sort_keys=True) + "\n" for record in records)
    )
    summary = compare(records, expected)

    nextest_output = run(
        ["cargo", "nextest", "run", "-p", "cargo-val", "--test", "c7_fixture", "--no-capture"],
        "nextest-control",
    )
    nextest_names = [
        f"suite_{name}"
        for name in ("alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta")
    ]
    observed = [name for name in nextest_names if f"c7_fixture {name}" in nextest_output]
    if len(observed) != 7:
        raise ValueError(f"nextest control expected 7 fixture names, found {len(observed)}")
    if "7 tests run: 7 passed, 0 skipped" not in nextest_output:
        raise ValueError("nextest control did not report exactly seven passes")
    summary["nextest_fixture_names"] = 7
    summary["nextest_passed"] = 7

    mutated = [dict(record) for record in records]
    mutated[0] = dict(mutated[0], name="suite::mutated")
    try:
        compare(mutated, expected)
    except ValueError:
        summary["mutated_identity_rejected"] = 1
    else:
        raise ValueError("mutated identity was accepted")
    (EVIDENCE / "acceptance-summary.json").write_text(json.dumps(summary, sort_keys=True) + "\n")
    print(json.dumps(summary, sort_keys=True))


if __name__ == "__main__":
    main()
