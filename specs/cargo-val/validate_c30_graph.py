#!/usr/bin/env python3
"""Replay graph records and check cargo-val's normal build graph by target."""

import argparse
import gzip
import json
from pathlib import Path
import subprocess
import sys
import tomllib


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


def exact_error_marker_count(output: str, marker: str) -> int:
    return output.count(marker)


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
    fixtures = [
        record
        for record in oracle_records
        if isinstance(record.get("input"), dict) and record["input"].get("target") == target
    ]
    if len(fixtures) != 1:
        raise RuntimeError(f"expected one target fixture for {target}, found {len(fixtures)}")
    fixture_case = fixtures[0].get("case")
    if not isinstance(fixture_case, str) or not fixture_case.endswith("_fixture"):
        raise RuntimeError("target fixture case must end in _fixture")
    control_case = f"{fixture_case.removesuffix('_fixture')}_negative_control"
    controls = [record for record in oracle_records if record.get("case") == control_case]
    if len(oracle_records) != 2 or len(fixtures) != 1 or len(controls) != 1:
        raise RuntimeError("target oracle must contain exactly one fixture and one control")
    fixture = fixtures[0].get("input")
    control = controls[0].get("control")
    if not isinstance(fixture, dict) or not isinstance(control, dict):
        raise RuntimeError("target oracle fixture and control must be objects")
    expected = fixture.get("expected")
    if not isinstance(expected, dict):
        raise RuntimeError("target oracle expected result must be an object")
    if "features" not in fixture or "exit" not in expected:
        raise RuntimeError("target oracle must bind features and exit explicitly")
    expected_artifacts = expected.get("cargo_val_library_artifacts")
    expected_errors = expected.get("product_compiler_errors")
    expected_exit = expected.get("exit")
    if expected_artifacts != 1 or expected_errors != 0 or expected_exit != 0:
        raise RuntimeError("expected target graph differs from the oracle contract")
    features = fixture.get("features")
    if not isinstance(features, list) or not all(isinstance(feature, str) for feature in features):
        raise RuntimeError("target features must be a string list")
    if control != {
        "mutation": "remove target from installed-target preflight list",
        "expected_rejected": True,
    }:
        raise RuntimeError("negative control differs from the target oracle")

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

    feature_argument = ",".join(features)
    command = [
        "cargo",
        "check",
        "-p",
        "cargo-val",
        "--lib",
        "--no-default-features",
        "--features",
        feature_argument,
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
        expected_artifacts,
        expected_errors,
        expected_exit,
    ):
        raise RuntimeError("Cargo target graph differs from its literal oracle")

    print(json.dumps({"target": target, "library_artifacts": len(artifacts), "product_errors": len(errors)}))
    print(json.dumps({"rejected_missing_target_preflight_controls": int(control_rejected)}))


def check_std_control(target: str, oracle_path: Path) -> None:
    oracle_records = read_records(oracle_path)
    fixtures = [record for record in oracle_records if record.get("case") == "c30b_fixture"]
    controls = [record for record in oracle_records if record.get("case") == "c30b_negative_control"]
    if len(oracle_records) != 2 or len(fixtures) != 1 or len(controls) != 1:
        raise RuntimeError("C30b oracle must contain exactly one fixture and one control")
    fixture = fixtures[0].get("input")
    control = controls[0].get("control")
    if not isinstance(fixture, dict) or not isinstance(control, dict):
        raise RuntimeError("C30b oracle fixture and control must be objects")
    dependency = fixture.get("normal_dependency")
    expected = fixture.get("expected")
    if fixture.get("target") != target or not isinstance(dependency, dict) or not isinstance(expected, dict):
        raise RuntimeError("target or fixture fields differ from the C30b oracle")
    if dependency != {"name": "serde", "features": ["std"]}:
        raise RuntimeError("normal dependency differs from the C30b oracle")
    expected_marker = expected.get("exact_error_marker")
    if expected != {
        "control_exit": 101,
        "exact_error_marker": "error[E0463]: can't find crate for `std`",
        "exact_error_marker_count": 2,
        "compiler_error_records": 5829,
        "control_diagnostics_in_product_count": 0,
    }:
        raise RuntimeError("expected control result differs from the C30b oracle")
    if control != {
        "mutation": "replace E0463 marker with unrelated compiler error while retaining exit 101",
        "expected_rejected": True,
    }:
        raise RuntimeError("negative control differs from the C30b oracle")

    add = None
    try:
        add = subprocess.run(
            ["cargo", "add", "serde", "--no-default-features", "--features", "std"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        if add.returncode != 0:
            raise RuntimeError(f"cargo add serde std control failed:\n{add.stderr}")
        result = subprocess.run(
            [
                "cargo",
                "check",
                "-p",
                "cargo-val",
                "--lib",
                "--no-default-features",
                "--features",
                "proxima-codec,proxima-primitives",
                "--target",
                target,
                "--message-format=json",
            ],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
    finally:
        if add is not None:
            remove = subprocess.run(
                ["cargo", "remove", "serde"],
                cwd=ROOT,
                capture_output=True,
                text=True,
                check=False,
            )
            manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
            normal_dependencies = manifest.get("dependencies", {})
            if "serde" in normal_dependencies:
                raise RuntimeError(f"cargo remove serde std control failed:\n{remove.stderr}")
    evidence_directory = ROOT / "specs" / "cargo-val" / "evidence" / "c30-graph"
    evidence_directory.mkdir(parents=True, exist_ok=True)
    with gzip.open(evidence_directory / f"{target}.std-control.cargo.jsonl.gz", "wt") as evidence_file:
        evidence_file.write(result.stdout)
    (evidence_directory / f"{target}.std-control.stderr.txt").write_text(result.stderr)
    if result.returncode != expected["control_exit"]:
        raise RuntimeError(f"std control exit differs from oracle: {result.returncode}")
    combined_output = result.stdout + result.stderr
    marker_count = exact_error_marker_count(combined_output, expected_marker)
    if marker_count != expected["exact_error_marker_count"]:
        raise RuntimeError(f"exact std error markers differ from oracle: {marker_count}")

    mutated_output = combined_output.replace(
        expected_marker,
        "error[E0000]: unrelated compiler error",
        marker_count,
    )
    mutation_rejected = (
        exact_error_marker_count(mutated_output, expected_marker)
        != expected["exact_error_marker_count"]
    )
    if control.get("expected_rejected") is not mutation_rejected:
        raise RuntimeError("unrelated-error mutation control was not rejected")
    control_records: list[dict[str, object]] = []
    for line_number, line in enumerate(result.stdout.splitlines(), start=1):
        try:
            record = json.loads(line)
        except json.JSONDecodeError as error:
            raise RuntimeError(f"std control Cargo output line {line_number} is not JSON: {error}") from error
        if isinstance(record, dict):
            control_records.append(record)
    product_errors = product_error_count(control_records)
    if product_errors != expected["control_diagnostics_in_product_count"]:
        raise RuntimeError(f"control diagnostics were counted as product errors: {product_errors}")
    compiler_errors = sum(
        record.get("reason") == "compiler-message"
        and isinstance(record.get("message"), dict)
        and record["message"].get("level") == "error"
        for record in control_records
    )
    if compiler_errors != expected["compiler_error_records"]:
        raise RuntimeError(f"compiler error records differ from oracle: {compiler_errors}")
    if marker_count != expected["exact_error_marker_count"]:
        raise RuntimeError(f"exact std error markers differ from oracle: {marker_count}")
    print(json.dumps({"std_control_exit": result.returncode, "exact_E0463_markers": marker_count}))
    print(json.dumps({"compiler_error_records": compiler_errors}))
    print(json.dumps({"control_diagnostics_in_product_count": product_errors}))
    print(json.dumps({"rejected_unrelated_error_mutation_controls": int(mutation_rejected)}))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--replay", type=Path)
    parser.add_argument("--target")
    parser.add_argument("--oracle", type=Path)
    parser.add_argument("--product-only", action="store_true")
    parser.add_argument("--std-control", action="store_true")
    arguments = parser.parse_args()
    if arguments.replay is not None and arguments.target is None and not arguments.product_only:
        replay(arguments.replay)
        return 0
    if arguments.target is not None and arguments.product_only and arguments.replay is None and arguments.oracle is not None:
        check_target(arguments.target, arguments.oracle)
        return 0
    if arguments.target is not None and arguments.std_control and arguments.replay is None and arguments.oracle is not None:
        check_std_control(arguments.target, arguments.oracle)
        return 0
    parser.error("select --replay PATH or --target TARGET --product-only")
    return 2


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(error, file=sys.stderr)
        sys.exit(1)
