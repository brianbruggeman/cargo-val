from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[2]
ORACLE = Path(__file__).resolve().parent / "oracles" / "c2.jsonl"
EXPECTED = Path(__file__).resolve().parent / "oracles" / "c2.expected.jsonl"
RAW_CONTROL = Path(__file__).resolve().parent / "evidence" / "c2" / "cargo-all-targets.jsonl"
ESCAPED_ORACLE = Path(__file__).resolve().parent / "oracles" / "c2-windows-escaped.jsonl"
ESCAPED_EXPECTED = Path(__file__).resolve().parent / "oracles" / "c2-windows-escaped.expected.jsonl"
OVER_DEPTH_ORACLE = Path(__file__).resolve().parent / "oracles" / "c2-over-depth.jsonl"
ESCAPED_FIELDS_ORACLE = Path(__file__).resolve().parent / "oracles" / "c2-escaped-fields.jsonl"
ESCAPED_FIELDS_EXPECTED = Path(__file__).resolve().parent / "oracles" / "c2-escaped-fields.expected.jsonl"


def load_jsonl(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def artifact_fields(record: dict) -> tuple[str, tuple[str, ...], bool, bool, str]:
    if record.get("reason") != "compiler-artifact":
        raise ValueError("record is not a Cargo compiler-artifact")
    target = record.get("target")
    profile = record.get("profile")
    if not isinstance(target, dict) or not isinstance(profile, dict):
        raise ValueError("Cargo target or profile is absent")
    name = target.get("name")
    kind = target.get("kind")
    target_test = target.get("test")
    profile_test = profile.get("test")
    executable = record.get("executable")
    filenames = record.get("filenames")
    if (
        not isinstance(name, str)
        or not isinstance(kind, list)
        or not all(isinstance(item, str) for item in kind)
        or not isinstance(target_test, bool)
        or not isinstance(profile_test, bool)
        or not isinstance(executable, str)
        or not executable
        or not isinstance(filenames, list)
        or executable not in filenames
    ):
        raise ValueError("Cargo artifact has an invalid target, profile, or executable field")
    return name, tuple(kind), target_test, profile_test, executable


def artifacts(records: list[dict]) -> list[dict]:
    return [record for record in records if record.get("reason") == "compiler-artifact"]


def signatures(records: list[dict]) -> list[tuple[str, tuple[str, ...], bool, bool]]:
    return sorted(artifact_fields(record)[:4] for record in records)


def selected(records: list[dict]) -> list[str]:
    return [artifact_fields(record)[4] for record in records if artifact_fields(record)[3]]


def json_depth(value: object) -> int:
    if isinstance(value, dict):
        return 1 + max((json_depth(item) for item in value.values()), default=0)
    if isinstance(value, list):
        return 1 + max((json_depth(item) for item in value), default=0)
    return 0


def escaped_controls(
    expected_path: Path,
    borrowed_oracle_path: Path,
    borrowed_oracle: list[dict],
) -> dict[str, int]:
    escaped_lines = ESCAPED_ORACLE.read_bytes().splitlines()
    expected_rows = load_jsonl(expected_path)
    over_depth_lines = OVER_DEPTH_ORACLE.read_bytes().splitlines()
    if len(escaped_lines) != 1 or len(expected_rows) != 1 or len(over_depth_lines) != 1:
        raise ValueError("escaped, expected, and over-depth controls require one record each")

    escaped_line = escaped_lines[0]
    escaped = json.loads(escaped_line)
    expected = expected_rows[0]
    over_depth_line = over_depth_lines[0]
    over_depth = json.loads(over_depth_line)
    decoded = artifact_fields(escaped)[4]
    borrowed = artifact_fields(borrowed_oracle[0])[4]
    borrowed_lines = borrowed_oracle_path.read_bytes().splitlines()
    if borrowed.encode() not in borrowed_lines[0]:
        raise ValueError("borrowed-path control is escaped in the literal input")
    if decoded.encode() in escaped_line:
        raise ValueError("escaped-path control already contains the decoded path")
    if json.dumps(decoded).encode() not in escaped_line:
        raise ValueError("escaped-path control does not contain the JSON-escaped path")

    observed = {
        "decoded_executable": decoded,
        "decoded_utf8_hex": decoded.encode().hex(),
        "max_record_bytes": len(escaped_line),
        "max_json_depth": json_depth(escaped),
        "scratch_capacity_bytes": len(decoded.encode()),
        "short_scratch_bytes": len(decoded.encode()) - 1,
        "over_byte_record_bytes": len(escaped_line + b" "),
        "over_depth_record_depth": json_depth(over_depth),
    }
    required_parser_targets = {
        "decoded_storage": "caller_scratch",
        "borrowed_storage": "input",
        "unknown_fields_ignored_without_allocation": int("future" in escaped),
        "parser_allocations_borrowed": 0,
        "parser_allocations_escaped": 0,
        "over_byte_limit_rejected": 1,
        "over_depth_limit_rejected": 1,
        "short_scratch_rejected": 1,
    }
    observed_keys = set(observed)
    target_keys = set(required_parser_targets)
    if set(expected) != observed_keys | target_keys:
        raise ValueError("escaped-path oracle fields differ from the admitted C2 contract")
    if any(expected[key] != value for key, value in observed.items()):
        raise ValueError(f"escaped-path oracle mismatch: observed={observed!r}, expected={expected!r}")
    if any(expected[key] != value for key, value in required_parser_targets.items()):
        raise ValueError("escaped-path parser targets differ from the admitted C2 contract")
    if len(over_depth_line) > expected["max_record_bytes"]:
        raise ValueError("over-depth control also exceeds the byte limit")
    if json_depth(over_depth) != expected["max_json_depth"] + 1:
        raise ValueError("over-depth control is not exactly one level too deep")
    if json.loads(escaped_line + b" ") != escaped:
        raise ValueError("over-byte control is not valid JSON")
    return {
        "escaped_windows_decoded_controls": 1,
        "borrowed_path_controls": 1,
        "over_byte_probe_shapes": 1,
        "over_depth_probe_shapes": 1,
        "short_scratch_probe_shapes": 1,
    }


def main(oracle_path: Path, escaped_expected_path: Path) -> int:
    oracle = load_jsonl(oracle_path)
    expected_rows = load_jsonl(EXPECTED)
    if len(oracle) != 3 or len(expected_rows) != 1:
        raise ValueError("C2 requires three input records and one expected record")

    expected = expected_rows[0]
    raw_control = artifacts(load_jsonl(RAW_CONTROL))
    if len(raw_control) != 3 or signatures(raw_control) != signatures(oracle):
        raise ValueError("retained all-targets Cargo records differ from the literal input shape")

    emitted = selected(oracle)
    ignored = len(oracle) - len(emitted)
    actual_indices = {index for index, record in enumerate(oracle) if artifact_fields(record)[3]}
    target_test_indices = {index for index, record in enumerate(oracle) if artifact_fields(record)[2]}
    kind_test_indices = {index for index, record in enumerate(oracle) if "test" in artifact_fields(record)[1]}
    rejected = int(target_test_indices != actual_indices) + int(kind_test_indices != actual_indices)
    observed = {
        "emitted_executables": emitted,
        "ignored_non_test_artifacts": ignored,
        "negative_controls_rejected": rejected,
    }
    if observed != expected:
        raise ValueError(f"literal C2 oracle mismatch: observed={observed!r}, expected={expected!r}")
    escaped_counts = escaped_controls(escaped_expected_path, oracle_path, oracle)
    escaped_fields = json.loads(ESCAPED_FIELDS_ORACLE.read_bytes())
    escaped_fields_expected = load_jsonl(ESCAPED_FIELDS_EXPECTED)
    if len(escaped_fields_expected) != 1:
        raise ValueError("escaped-field expected oracle requires exactly one record")
    escaped_fields_observed = {
        "reason": escaped_fields.get("reason"),
        "profile_test": escaped_fields.get("profile", {}).get("test"),
        "executable": escaped_fields.get("executable"),
    }
    if escaped_fields_observed != escaped_fields_expected[0]:
        raise ValueError("escaped-field JSON oracle does not decode to its expected Cargo fields")

    environment = os.environ.copy()
    environment["RUSTC_WRAPPER"] = ""
    cargo = subprocess.run(
        ["cargo", "test", "--no-run", "--bin", "cargo-val", "--test", "cli_smoke", "--message-format=json"],
        cwd=ROOT,
        env=environment,
        capture_output=True,
        text=True,
        check=True,
    )
    generated_records = []
    for line in cargo.stdout.splitlines():
        if not line.strip():
            continue
        record = json.loads(line)
        if record.get("reason") == "compiler-artifact":
            generated_records.append(record)
    generated = [
        record for record in generated_records
        if record.get("target", {}).get("name") in {"cargo-val", "cli_smoke"}
    ]
    if len(generated) != 3 or signatures(generated) != signatures(raw_control):
        raise ValueError("live Cargo records differ from the retained all-targets control")

    print(json.dumps({
        "literal_test_executables": len(emitted),
        "literal_non_test_ignored": ignored,
        "raw_cargo_test_artifacts": len(selected(raw_control)),
        "raw_cargo_non_test_artifacts": len(raw_control) - len(selected(raw_control)),
        "live_cargo_test_artifacts": len(selected(generated)),
        "live_cargo_non_test_artifacts": len(generated) - len(selected(generated)),
        "naive_selectors_rejected": rejected,
        "escaped_field_spellings": 1,
        **escaped_counts,
    }, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        parser = argparse.ArgumentParser()
        parser.add_argument("--oracle", type=Path, default=ORACLE)
        parser.add_argument("--escaped-expected", type=Path, default=ESCAPED_EXPECTED)
        arguments = parser.parse_args()
        raise SystemExit(main(arguments.oracle, arguments.escaped_expected))
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"C2 oracle validation failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
