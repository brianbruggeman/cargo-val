#!/usr/bin/env python3
"""Exercise generated defaults, overrides, and validation through the build script."""

import os
from pathlib import Path
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[2]
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
if not TARGET.is_absolute():
    TARGET = ROOT / TARGET


def run_build(environment: dict[str, str | None]) -> subprocess.CompletedProcess[str]:
    process_environment = os.environ.copy()
    process_environment.pop("CARGO_VAL_BUILD_JOBS", None)
    process_environment.pop("CARGO_VAL_BUILD_COLOR", None)
    for key, value in environment.items():
        if value is None:
            process_environment.pop(key, None)
        else:
            process_environment[key] = value

    return subprocess.run(
        ["cargo", "check", "-p", "cargo-val", "--lib", "--no-default-features"],
        cwd=ROOT,
        env=process_environment,
        capture_output=True,
        text=True,
        check=False,
    )


def generated_file() -> Path:
    build_directories = sorted(generated_artifacts(), key=lambda path: path.stat().st_mtime_ns, reverse=True)
    if not build_directories:
        raise RuntimeError("cargo-val build script did not emit its constants")
    return build_directories[0]


def generated_artifacts() -> list[Path]:
    return sorted((TARGET / "debug" / "build").glob("cargo-val-*/out/cargo_val_build.rs"))


def artifact_snapshot() -> dict[Path, tuple[int, bytes]]:
    return {
        path: (path.stat().st_mtime_ns, path.read_bytes())
        for path in generated_artifacts()
    }


def constants(source: str) -> tuple[int, bool]:
    jobs = re.search(r"pub const JOBS: usize = (\d+);", source)
    color = re.search(r"pub const COLOR: bool = (true|false);", source)
    if jobs is None or color is None:
        raise RuntimeError(f"generated constants missing or malformed:\n{source}")
    return int(jobs.group(1)), color.group(1) == "true"


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def main() -> int:
    defaults = run_build({})
    require(defaults.returncode == 0, defaults.stderr)
    default_source = generated_file().read_text()
    require(constants(default_source) == (2, True), "default constants differ from oracle")

    overridden = run_build(
        {
            "CARGO_VAL_BUILD_JOBS": "2",
            "CARGO_VAL_BUILD_COLOR": "false",
        }
    )
    require(overridden.returncode == 0, overridden.stderr)
    override_source = generated_file().read_text()
    require(
        constants(override_source) == (2, False),
        "overridden constants differ from oracle",
    )

    before_invalid = artifact_snapshot()
    invalid = run_build({"CARGO_VAL_BUILD_JOBS": "0"})
    require(invalid.returncode != 0, "jobs=0 unexpectedly passed build validation")
    require(
        'path: "jobs", message: "must be greater than zero"' in invalid.stderr,
        f"jobs validation error missing from build output:\n{invalid.stderr}",
    )
    require(
        artifact_snapshot() == before_invalid,
        "invalid settings created or changed a generated constants artifact",
    )

    print("default artifact payload:")
    print(default_source, end="")
    print("override artifact payload:")
    print(override_source, end="")
    print("default constants: 2")
    print("overridden constants: 2")
    print("jobs=0 validation failures: 1")
    print("invalid generated artifacts: 0")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(error, file=sys.stderr)
        sys.exit(1)
