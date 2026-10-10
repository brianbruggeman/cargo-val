#!/usr/bin/env python3
"""Replay graph records and check cargo-val's normal build graph by target."""

import argparse
import json
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[2]
PACKAGE_ID_PREFIX = f"path+file://{ROOT}#"


def read_records(path: Path) -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    for line_number, line in enumerate(path.read_text().splitlines(), start=1):
        try:
            record = json.loads(line)
        except json.JSONDecodeError as error:
            raise RuntimeError(f"{path}:{line_number}: invalid JSON: {error}") from error
        if not isinstance(record, dict):
            raise RuntimeError(f"{path}:{line_number}: record must be an object")
        records.append(record)
    return records


def is_library_artifact(record: dict[str, object]) -> bool:
    target = record.get("target")
    if not isinstance(target, dict):
        return False
    kinds = target.get("kind")
    filenames = record.get("filenames")
    return (
        record.get("reason") == "compiler-artifact"
        and is_cargo_val_package(record.get("package_id"))
        and target.get("name") == "cargo_val"
        and isinstance(kinds, list)
        and "lib" in kinds
        and isinstance(filenames, list)
        and any(isinstance(name, str) and name.endswith(".rmeta") for name in filenames)
    )


def is_cargo_val_package(package_id: object) -> bool:
    return isinstance(package_id, str) and (
        package_id.startswith(PACKAGE_ID_PREFIX) or package_id.startswith("cargo-val ")
    )


def require_installed_target(target: str, installed_targets: list[str]) -> None:
    if target not in installed_targets:
        raise RuntimeError(f"target {target} is not installed")


def product_error_count(records: list[dict[str, object]]) -> int:
    return sum(
        record.get("reason") == "compiler-message"
        and is_cargo_val_package(record.get("package_id"))
        and isinstance(record.get("message"), dict)
        and record["message"].get("level") == "error"
        for record in records
    )


def validate_replay(records: list[dict[str, object]]) -> tuple[int, int]:
    if len(records) != 2:
        raise RuntimeError(f"expected exactly 2 oracle records, found {len(records)}")
    fixtures = [record for record in records if record.get("case") == "c30g_fixture"]
    controls = [record for record in records if record.get("case") == "c30g_negative_control"]
    if len(fixtures) != 1 or len(controls) != 1:
        raise RuntimeError(f"expected 1 fixture and 1 control, found {len(fixtures)} and {len(controls)}")

    fixture = fixtures[0]
    payload = fixture.get("input")
    expected = fixture.get("expected")
    if not isinstance(payload, dict) or not isinstance(expected, dict):
        raise RuntimeError("fixture must contain input and expected objects")
    observed = int(is_library_artifact(payload))
    product_errors = product_error_count([payload])
    if observed != expected.get("library_artifacts") or product_errors != expected.get("product_errors"):
        raise RuntimeError(
            f"fixture mismatch: library_artifacts={observed}, product_errors={product_errors}"
        )
    return observed, product_errors


def replay(path: Path) -> None:
    records = read_records(path)
    observed, product_errors = validate_replay(records)
    fixture_record = next(record for record in records if record.get("case") == "c30g_fixture")
    payload = fixture_record.get("input")
    if not isinstance(payload, dict):
        raise RuntimeError("fixture payload is not an object")
    controls = [record for record in records if record.get("case") == "c30g_negative_control"]
    control_record = controls[0]

    control = control_record.get("control")
    if not isinstance(control, dict) or control.get("mutation") != "remove target.kind from compiler-artifact record":
        raise RuntimeError("control mutation differs from the literal oracle")
    mutated = dict(payload)
    target = payload.get("target")
    if not isinstance(target, dict):
        raise RuntimeError("positive fixture has no target object")
    mutated["target"] = {key: value for key, value in target.items() if key != "kind"}
    if is_library_artifact(mutated) or control.get("expected_rejected") is not True:
        raise RuntimeError("missing target.kind control was not rejected")
    error_record = control.get("compiler_error_record")
    expected_errors = control.get("expected_product_errors")
    if not isinstance(error_record, dict) or product_error_count([error_record]) != expected_errors:
        raise RuntimeError("nested cargo compiler error control was not counted")
    unexpected_record = control.get("unexpected_event_record")
    if not isinstance(unexpected_record, dict):
        raise RuntimeError("unexpected-event control is missing its literal compiler event")
    try:
        validate_replay(records + [unexpected_record])
    except RuntimeError:
        pass
    else:
        raise RuntimeError("unexpected compiler event was ignored by replay")
    unrelated = dict(payload)
    unrelated["package_id"] = "other-package 1.0.0"
    if is_library_artifact(unrelated):
        raise RuntimeError("artifact from another package was admitted as cargo-val")

    print(json.dumps({"observed_library_artifacts": observed, "product_errors": product_errors}))
    print(json.dumps({"rejected_missing_target_kind_controls": 1}))
    print(json.dumps({"rejected_unrelated_package_controls": 1}))
    print(json.dumps({"counted_nested_compiler_error_controls": 1}))
    print(json.dumps({"rejected_unexpected_event_controls": 1}))


def check_target(target: str, oracle_path: Path) -> None:
    oracle_records = read_records(oracle_path)
    fixtures = [record for record in oracle_records if record.get("case") == "c30a_fixture"]
    controls = [record for record in oracle_records if record.get("case") == "c30a_negative_control"]
    if len(oracle_records) != 2 or len(fixtures) != 1 or len(controls) != 1:
        raise RuntimeError("C30a oracle must contain exactly one fixture and one control")
    fixture = fixtures[0].get("input")
    control = controls[0].get("control")
    if not isinstance(fixture, dict) or not isinstance(control, dict):
        raise RuntimeError("C30a oracle fixture and control must be objects")
    expected = fixture.get("expected")
    if not isinstance(expected, dict) or fixture.get("target") != target:
        raise RuntimeError("target differs from the C30a fixture")
    if fixture.get("features") != ["proxima-codec", "proxima-primitives"]:
        raise RuntimeError("features differ from the C30a fixture")
    if expected != {"cargo_val_library_artifacts": 1, "product_compiler_errors": 0, "exit": 0}:
        raise RuntimeError("expected product graph differs from the C30a fixture")
    if control != {
        "mutation": "remove target from installed-target preflight list",
        "expected_rejected": True,
    }:
        raise RuntimeError("negative control differs from the C30a oracle")

    installed = subprocess.run(
        ["rustup", "target", "list", "--installed"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if installed.returncode != 0:
        raise RuntimeError(f"rustup target preflight failed:\n{installed.stderr}")
    installed_targets = installed.stdout.splitlines()
    require_installed_target(target, installed_targets)
    control_targets = [installed_target for installed_target in installed_targets if installed_target != target]
    try:
        require_installed_target(target, control_targets)
    except RuntimeError as error:
        control_rejected = str(error) == f"target {target} is not installed"
    else:
        control_rejected = False
    if control.get("expected_rejected") is not control_rejected:
        raise RuntimeError("missing-target preflight control did not match its oracle")

    features = ",".join(fixture["features"])
    command = [
        "cargo",
        "check",
        "-p",
        "cargo-val",
        "--lib",
        "--no-default-features",
        "--features",
        features,
        "--target",
        target,
        "--message-format=json",
    ]
    result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=False)
    evidence_directory = ROOT / "specs" / "cargo-val" / "evidence" / "c30-graph"
    evidence_directory.mkdir(parents=True, exist_ok=True)
    (evidence_directory / f"{target}.cargo.jsonl").write_text(result.stdout)
    (evidence_directory / f"{target}.stderr.txt").write_text(result.stderr)
    messages: list[dict[str, object]] = []
    for line_number, line in enumerate(result.stdout.splitlines(), start=1):
        try:
            message = json.loads(line)
        except json.JSONDecodeError as error:
            raise RuntimeError(f"cargo output line {line_number} is not JSON: {error}") from error
        if isinstance(message, dict):
            messages.append(message)

    package_id = next(
        (
            message.get("package_id")
            for message in messages
            if message.get("reason") == "compiler-artifact"
            and isinstance(message.get("target"), dict)
            and message["target"].get("name") == "cargo_val"
            and is_cargo_val_package(message.get("package_id"))
        ),
        None,
    )
    artifacts = [message for message in messages if message.get("package_id") == package_id and is_library_artifact(message)]
    errors = [
        message
        for message in messages
        if message.get("reason") == "compiler-message"
        and message.get("package_id") == package_id
        and isinstance(message.get("message"), dict)
        and message["message"].get("level") == "error"
    ]
    finished = [
        message
        for message in messages
        if message.get("reason") == "build-finished" and message.get("success") is True
    ]
    if result.returncode != 0 or package_id is None or len(artifacts) != 1 or errors or len(finished) != 1:
        raise RuntimeError(
            f"product graph check failed: exit={result.returncode}, library_artifacts={len(artifacts)}, "
            f"product_errors={len(errors)}, successful_build_records={len(finished)}\n{result.stderr}"
        )

    if (len(artifacts), len(errors), result.returncode) != (
        expected["cargo_val_library_artifacts"],
        expected["product_compiler_errors"],
        expected["exit"],
    ):
        raise RuntimeError("Cargo target graph differs from the C30a literal oracle")

    print(json.dumps({"target": target, "library_artifacts": len(artifacts), "product_errors": len(errors)}))
    print(json.dumps({"rejected_missing_target_preflight_controls": int(control_rejected)}))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--replay", type=Path)
    parser.add_argument("--target")
    parser.add_argument("--oracle", type=Path)
    parser.add_argument("--product-only", action="store_true")
    arguments = parser.parse_args()
    if arguments.replay is not None and arguments.target is None and not arguments.product_only:
        replay(arguments.replay)
        return 0
    if arguments.target is not None and arguments.product_only and arguments.replay is None and arguments.oracle is not None:
        check_target(arguments.target, arguments.oracle)
        return 0
    parser.error("select --replay PATH or --target TARGET --product-only")
    return 2


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(error, file=sys.stderr)
        sys.exit(1)
