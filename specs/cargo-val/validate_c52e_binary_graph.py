#!/usr/bin/env python3
"""Build cargo-val and verify its production feature graph rejects std."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
FEATURES = "proxima-core,proxima-codec,proxima-primitives"


def run(command: list[str], *, cwd: Path = ROOT) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=cwd,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=os.environ.copy(),
    )


def std_feature_lines(graph: str) -> int:
    return sum(
        "std" in line.rsplit(" ", maxsplit=1)[-1].strip("{} ").split(",")
        for line in graph.splitlines()
    )


def main() -> int:
    build = run(
        [
            "cargo",
            "build",
            "-p",
            "cargo-val",
            "--release",
            "--no-default-features",
            "--features",
            FEATURES,
            "--message-format=json-render-diagnostics",
        ]
    )
    if build.returncode != 0:
        print(build.stderr, end="")
        return build.returncode
    executables = [
        record["executable"]
        for line in build.stdout.splitlines()
        if line.startswith("{")
        for record in [json.loads(line)]
        if record.get("reason") == "compiler-artifact"
        and record.get("target", {}).get("name") == "cargo-val"
        and "bin" in record.get("target", {}).get("kind", [])
        and record.get("executable")
    ]
    if len(executables) != 1:
        print(json.dumps({"release_executables": len(executables)}))
        return 1

    production_graph = run(
        [
            "cargo",
            "tree",
            "-p",
            "cargo-val",
            "--no-default-features",
            "--features",
            FEATURES,
            "-e",
            "normal",
            "-f",
            "{p} {f}",
        ]
    )
    if production_graph.returncode != 0:
        print(production_graph.stderr, end="")
        return production_graph.returncode
    production_std_lines = std_feature_lines(production_graph.stdout)

    with tempfile.TemporaryDirectory(prefix="cargo-val-c52e-control-") as temporary:
        temporary_root = Path(temporary)
        (temporary_root / "src").mkdir()
        (temporary_root / "Cargo.toml").write_text(
            '[package]\nname = "std-feature-control"\nversion = "0.1.0"\n'
            'edition = "2024"\n\n[dependencies]\n'
            'serde = { version = "1", features = ["std"] }\n',
            encoding="utf-8",
        )
        (temporary_root / "src" / "lib.rs").write_text("pub fn control() {}\n", encoding="utf-8")
        control_graph = run(
            ["cargo", "tree", "--manifest-path", str(temporary_root / "Cargo.toml"), "-e", "normal", "-f", "{p} {f}"],
            cwd=temporary_root,
        )
    if control_graph.returncode != 0:
        print(control_graph.stderr, end="")
        return control_graph.returncode
    control_std_lines = std_feature_lines(control_graph.stdout)

    result = {
        "release_executables": len(executables),
        "production_package_lines": len(production_graph.stdout.splitlines()),
        "production_std_feature_lines": production_std_lines,
        "control_std_feature_lines": control_std_lines,
        "control_rejected": int(control_std_lines > 0),
    }
    print(json.dumps(result, separators=(",", ":")))
    return int(not (production_std_lines == 0 and control_std_lines > 0))


if __name__ == "__main__":
    raise SystemExit(main())
