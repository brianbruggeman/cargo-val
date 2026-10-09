from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys


PIN = "5207d66e9995ffcb4d3b2d2258c02e6fabc4cc6b"
PROXIMA_ROOT = Path(
    os.environ.get(
        "PROXIMA_ROOT",
        str(Path(__file__).resolve().parents[4] / "slot-0" / "proxima-windows"),
    )
)
ORACLE = Path(__file__).resolve().parent / "oracles" / "p0.jsonl"


def pinned_source(path: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(PROXIMA_ROOT), "show", f"{PIN}:{path}"],
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout


def line_with(source: str, pattern: str, path: str) -> int:
    for line_number, line in enumerate(source.splitlines(), start=1):
        if pattern in line:
            return line_number
    raise ValueError(f"missing pinned source anchor {pattern!r} in {path}")


def main() -> int:
    process_manifest = pinned_source("proxima-process/Cargo.toml")
    process_root = pinned_source("proxima-process/src/lib.rs")
    command = pinned_source("proxima-process/src/command.rs")
    unix_spawn = pinned_source("proxima-process/src/spawn.rs")
    windows_spawn = pinned_source("proxima-process/src/spawn_windows.rs")
    windows_reactor = pinned_source("prime/src/os/reactor_windows.rs")
    readiness = pinned_source("prime/src/os/readiness.rs")

    gaps = [
        {
            "id": "no_std_process_launch",
            "source": "proxima-process/src/spawn.rs",
            "line": line_with(unix_spawn, "pub fn wait", "proxima-process/src/spawn.rs"),
            "evidence": "the Unix child owner is a std process wrapper with blocking wait",
        },
        {
            "id": "child_stdio_readiness",
            "source": "proxima-process/src/command.rs",
            "line": line_with(command, "thread::Builder::new()", "proxima-process/src/command.rs"),
            "evidence": "child stdio is serviced by helper threads around blocking I/O",
        },
        {
            "id": "windows_process_wrapper",
            "source": "proxima-process/src/spawn_windows.rs",
            "line": line_with(windows_spawn, "use std::process", "proxima-process/src/spawn_windows.rs"),
            "evidence": "Windows child lifecycle wraps std::process without a reactor exit event",
        },
        {
            "id": "no_std_handle_reactor",
            "source": "prime/src/os/reactor_windows.rs",
            "line": line_with(windows_reactor, "AsSocket", "prime/src/os/reactor_windows.rs"),
            "evidence": "Windows reactor registration accepts sockets; child pipe/process handles are not exposed",
        },
    ]
    if 'default = ["std"]' not in process_manifest:
        raise ValueError("pinned process manifest no longer has the expected std default")
    if "feature = \"std\"" not in process_root:
        raise ValueError("pinned process root no longer gates host APIs on std")
    if "ReadyState::OffWorker" not in readiness or "wake_by_ref" not in readiness:
        raise ValueError("pinned readiness fallback changed; refresh the source gap inventory")
    if "ChildSession" in process_root + command + unix_spawn + windows_spawn:
        raise ValueError("pinned sources now contain ChildSession; refresh the source gap inventory")

    signature = "pub struct ChildSession<In, Out, Err> { /* persistent native state */ }"
    observed = gaps + [{"api_signature": signature}]
    expected = [json.loads(line) for line in ORACLE.read_text().splitlines() if line.strip()]
    if observed != expected:
        raise ValueError(
            f"source gaps or proposed session signature differ from the literal oracle: "
            f"observed={observed!r}, expected={expected!r}"
        )

    print(json.dumps({"revision": PIN}, sort_keys=True))
    for gap in gaps:
        print(json.dumps(gap, sort_keys=True))
    print(json.dumps({"api_signature": signature}, sort_keys=True))
    print(
        json.dumps(
            {
                "counts": {
                    "source_backed_gaps": len(gaps),
                    "typed_session_signatures": 1,
                    "blocking_calls_admitted": 0,
                }
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"process gap validation failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
