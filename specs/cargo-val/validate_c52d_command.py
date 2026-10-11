#!/usr/bin/env python3
"""Exercise the release no_std binary through Cargo's external-subcommand lookup."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import time


ROOT = Path(__file__).resolve().parents[2]
EVIDENCE = ROOT / "specs/cargo-val/evidence/c52d"
FIXTURE = ROOT / "tests/fixtures/cli_run/Cargo.toml"
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", "/tmp/cargo_val_c52d_probe_target"))
FEATURES = "proxima-core,proxima-codec,proxima-primitives"


def run(command: list[str], environment: dict[str, str], timeout: int = 180) -> dict:
    started = time.monotonic_ns()
    result = subprocess.run(command, cwd=ROOT, env=environment, capture_output=True, timeout=timeout)
    return {
        "command": command,
        "elapsed_ns": time.monotonic_ns() - started,
        "status": result.returncode,
        "stdout": result.stdout.decode("utf-8", "replace"),
        "stderr": result.stderr.decode("utf-8", "replace"),
    }


def main() -> None:
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(TARGET)
    build = run(
        ["cargo", "build", "--release", "-p", "cargo-val", "--bin", "cargo-val", "--no-default-features", "--features", FEATURES],
        environment,
    )
    if build["status"] != 0:
        raise AssertionError(f"release binary build failed: {build['stderr']}")

    binary = TARGET / "release/cargo-val"
    binary_sha256 = hashlib.sha256(binary.read_bytes()).hexdigest()
    environment["PATH"] = str(binary.parent) + os.pathsep + environment.get("PATH", "")
    environment["CARGO_VAL_JOBS"] = "2"
    environment.pop("CARGO_VAL_COLOR", None)
    command = ["cargo", "val", "--manifest-path", str(FIXTURE), "--lib"]

    without_color = environment.copy()
    without_color["NO_COLOR"] = ""
    without_color["CARGO_VAL_C52D_FAIL"] = "1"
    mixed = run(command, without_color)

    truncated_environment = without_color.copy()
    truncated_environment["CARGO_VAL_C52D_LONG_OUTPUT"] = "1"
    truncated = run(command, truncated_environment)

    with_color = environment.copy()
    with_color.pop("NO_COLOR", None)
    with_color.pop("CARGO_VAL_C52D_FAIL", None)
    colored = run(command, with_color)

    unsupported_ignored = run([*command, "--", "--ignored"], without_color)

    record = {
        "binary_sha256": binary_sha256,
        "fixture": str(FIXTURE),
        "build": build,
        "no_color_mixed": mixed,
        "no_color_truncated": truncated,
        "default_color_passing": colored,
        "unsupported_ignored_selection": unsupported_ignored,
    }
    (EVIDENCE / "command-acceptance.json").write_text(json.dumps(record, indent=2) + "\n")

    expected_mixed = {"fixture::fails_when_requested FAIL", "fixture::passes PASS", "fixture::ignored_by_default IGNORED"}
    mixed_lines = mixed["stdout"].splitlines()
    if mixed["status"] != 1 or set(mixed_lines) != expected_mixed or len(mixed_lines) != 3:
        raise AssertionError(f"mixed exact-child results differ: {mixed}")
    if "requested fixture failure" not in mixed["stderr"] or "stdout:\n" not in mixed["stderr"]:
        raise AssertionError(f"failing child output was not reported: {mixed}")
    if truncated["status"] != 1 or set(truncated["stdout"].splitlines()) != expected_mixed:
        raise AssertionError(f"long-output control changed child results: {truncated}")
    if "stdout:\n" not in truncated["stderr"] or "\n[stdout truncated]" not in truncated["stderr"]:
        raise AssertionError(f"long-output control did not show bounded capture: {truncated}")
    retained_stdout = truncated["stderr"].split("stdout:\n", 1)[1].split("\n[stdout truncated]", 1)[0]
    if len(retained_stdout.encode()) != 4096 or retained_stdout.count("X") < 3900:
        raise AssertionError(f"bounded capture retained unexpected bytes: {truncated}")
    if "\x1b" in mixed["stdout"] or "not implemented" in mixed["stdout"] + mixed["stderr"]:
        raise AssertionError(f"NO_COLOR or run-mode control failed: {mixed}")

    expected_colored = {
        "fixture::fails_when_requested \x1b[32mPASS\x1b[0m",
        "fixture::passes \x1b[32mPASS\x1b[0m",
        "fixture::ignored_by_default \x1b[33mIGNORED\x1b[0m",
    }
    colored_lines = colored["stdout"].splitlines()
    if colored["status"] != 0 or set(colored_lines) != expected_colored or len(colored_lines) != 3:
        raise AssertionError(f"default-color exact-child results differ: {colored}")
    expected_ignored = ["fixture::ignored_by_default FAIL"]
    if unsupported_ignored["status"] != 1 or unsupported_ignored["stdout"].splitlines() != expected_ignored:
        raise AssertionError(f"ignored-only selection did not run exactly the ignored child: {unsupported_ignored}")
    if "ignored fixture body should not run" not in unsupported_ignored["stderr"]:
        raise AssertionError(f"ignored-only selection skipped the ignored test body: {unsupported_ignored}")

    print(json.dumps({
        "release_executables": 1,
        "cargo_val_invocations": 4,
        "exact_child_results_mixed": len(mixed_lines),
        "nonzero_aggregate_exits": 1,
        "not_implemented_messages": 0,
        "no_color_ansi_sequences": mixed["stdout"].count("\x1b"),
        "default_colored_records": len(colored_lines),
        "failure_payloads_reported": mixed["stderr"].count("requested fixture failure"),
        "truncated_stdout_markers": truncated["stderr"].count("[stdout truncated]"),
        "truncated_stdout_retained_bytes": len(retained_stdout.encode()),
        "ignored_only_exact_child_results": len(expected_ignored),
        "binary_sha256": binary_sha256,
    }))


if __name__ == "__main__":
    main()
