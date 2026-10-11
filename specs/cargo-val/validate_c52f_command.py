#!/usr/bin/env python3
"""Install with manifest defaults, exercise cargo val, and retain a lookup control."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parents[2]
EVIDENCE = ROOT / "specs/cargo-val/evidence/c52f"


def run(command: list[str], *, cwd: Path, environment: dict[str, str]) -> dict:
    started_ns = time.monotonic_ns()
    process = subprocess.run(
        command,
        cwd=cwd,
        env=environment,
        capture_output=True,
        text=True,
        timeout=300,
        check=False,
    )
    return {
        "command": command,
        "status": process.returncode,
        "elapsed_ns": time.monotonic_ns() - started_ns,
        "stdout": process.stdout,
        "stderr": process.stderr,
    }


def std_feature_lines(graph: str) -> int:
    return sum(
        "std" in line.rsplit(" ", maxsplit=1)[-1].strip("{} ").split(",")
        for line in graph.splitlines()
    )


def main() -> None:
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    cargo = shutil.which("cargo")
    if cargo is None:
        raise AssertionError("cargo is present on the input PATH")
    base_environment = os.environ.copy()

    with tempfile.TemporaryDirectory(prefix="cargo-val-c52f-") as temporary:
        temporary_root = Path(temporary)
        install_root = temporary_root / "install"
        target_root = temporary_root / "target"
        fixture_root = temporary_root / "fixture"
        (fixture_root / "src").mkdir(parents=True)
        (fixture_root / "Cargo.toml").write_text(
            '[package]\nname = "cargo_val_c52f_fixture"\nversion = "0.1.0"\n'
            'edition = "2024"\n',
            encoding="utf-8",
        )
        (fixture_root / "src/lib.rs").write_text(
            '#[cfg(test)]\nmod fixture {\n'
            '    #[test]\n    fn passes() {}\n'
            '    #[test]\n    fn fails() { assert_eq!(1, 0); }\n}\n',
            encoding="utf-8",
        )

        install_environment = base_environment.copy()
        install_environment["CARGO_TARGET_DIR"] = str(target_root)
        install = run(
            [cargo, "install", "--path", str(ROOT), "--root", str(install_root), "--locked"],
            cwd=ROOT,
            environment=install_environment,
        )
        if install["status"] != 0:
            raise AssertionError(f"plain default-feature install failed: {install['stderr']}")

        executable = install_root / "bin/cargo-val"
        if not executable.is_file():
            raise AssertionError("cargo install produced cargo-val")
        binary_hash = hashlib.sha256(executable.read_bytes()).hexdigest()
        invocation_environment = base_environment.copy()
        invocation_environment["PATH"] = str(executable.parent) + os.pathsep + invocation_environment.get("PATH", "")
        invocation_environment["CARGO_TARGET_DIR"] = str(target_root)
        invocation_environment["CARGO_VAL_JOBS"] = "2"
        invocation_environment["NO_COLOR"] = ""
        invocation = run(
            [cargo, "val", "--manifest-path", str(fixture_root / "Cargo.toml"), "--lib", "fixture"],
            cwd=fixture_root,
            environment=invocation_environment,
        )
        expected = {"fixture::passes PASS", "fixture::fails FAIL"}
        actual = invocation["stdout"].splitlines()
        if invocation["status"] != 1 or len(actual) != 2 or set(actual) != expected:
            raise AssertionError(f"two exact fixture results differ: {invocation}")
        if "not implemented" in invocation["stdout"].lower() + invocation["stderr"].lower():
            raise AssertionError("installed default-feature runner is not implemented")

        graph = run(
            [cargo, "tree", "--manifest-path", str(ROOT / "Cargo.toml"), "-e", "normal", "-f", "{p} {f}"],
            cwd=ROOT,
            environment=install_environment,
        )
        if graph["status"] != 0:
            raise AssertionError(f"default normal feature graph failed: {graph['stderr']}")
        graph_std_lines = std_feature_lines(graph["stdout"])
        if graph_std_lines != 0:
            raise AssertionError(f"default normal graph has {graph_std_lines} std feature lines")

        isolated_path = temporary_root / "path-with-cargo-only"
        isolated_path.mkdir()
        (isolated_path / "cargo").symlink_to(cargo)
        control_environment = base_environment.copy()
        control_environment["PATH"] = str(isolated_path)
        control = run(
            [str(isolated_path / "cargo"), "val", "--help"],
            cwd=fixture_root,
            environment=control_environment,
        )
        resolved_cargo_val = shutil.which("cargo-val", path=str(isolated_path))
        if resolved_cargo_val is not None or control["status"] == 0:
            raise AssertionError(f"cargo-only PATH control unexpectedly resolved cargo-val: {control}")

        record = {
            "host": platform.system() + "-" + platform.machine(),
            "install": install,
            "installed_executable_sha256": binary_hash,
            "run": invocation,
            "expected_results": sorted(expected),
            "normal_graph_package_lines": len(graph["stdout"].splitlines()),
            "normal_graph": graph["stdout"],
            "normal_graph_std_feature_lines": graph_std_lines,
            "negative_control": control,
            "negative_control_resolved_cargo_val": resolved_cargo_val,
            "observed": {
                "installed_executables": 1,
                "cargo_val_invocations": 1,
                "exact_child_results": len(actual),
                "not_implemented_messages": 0,
                "normal_std_feature_lines": graph_std_lines,
                "isolated_path_resolved_cargo_val_commands": int(resolved_cargo_val is not None),
            },
        }
        (EVIDENCE / "command-acceptance.json").write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(record["observed"], separators=(",", ":")))


if __name__ == "__main__":
    main()
