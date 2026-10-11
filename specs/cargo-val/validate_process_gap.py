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


def anchor_rejected(source: str, pattern: str) -> bool:
    try:
        line_with(source, pattern, "negative-control-fixture")
    except ValueError:
        return True
    return False


def main() -> int:
    process_manifest = pinned_source("proxima-process/Cargo.toml")
    process_root = pinned_source("proxima-process/src/lib.rs")
    command = pinned_source("proxima-process/src/command.rs")
    unix_spawn = pinned_source("proxima-process/src/spawn.rs")
    windows_spawn = pinned_source("proxima-process/src/spawn_windows.rs")
    windows_reactor = pinned_source("prime/src/os/reactor_windows.rs")
    readiness = pinned_source("prime/src/os/readiness.rs")
    sync = pinned_source("proxima-primitives/src/sync/mod.rs")
    atomic_permits = pinned_source("proxima-primitives/src/sync/atomic_permits.rs")
    prime_root = pinned_source("prime/src/lib.rs")
    prime_manifest = pinned_source("prime/Cargo.toml")
    if not anchor_rejected("pub fn poll(&mut self) {}", "waitpid(self.pid, &mut status, 0)"):
        raise ValueError("negative control accepted a source without the blocking-wait anchor")

    gaps = [
        {
            "id": "no_std_process_launch",
            "source": "proxima-process/src/spawn.rs",
            "line": line_with(unix_spawn, "pub fn spawn(", "proxima-process/src/spawn.rs"),
            "related_sources": [
                {
                    "source": "proxima-process/src/lib.rs",
                    "line": line_with(process_root, "#[cfg(all(feature = \"std\", unix))]", "proxima-process/src/lib.rs"),
                }
            ],
            "evidence": "the only Unix launch module is gated on std",
        },
        {
            "id": "child_stdio_readiness",
            "source": "proxima-process/src/command.rs",
            "line": line_with(command, "thread::Builder::new()", "proxima-process/src/command.rs"),
            "related_sources": [
                {
                    "source": "prime/src/os/reactor_windows.rs",
                    "line": line_with(windows_reactor, "AsSocket", "prime/src/os/reactor_windows.rs"),
                },
                {
                    "source": "prime/src/os/readiness.rs",
                    "line": line_with(readiness, "to busy-poll", "prime/src/os/readiness.rs"),
                }
            ],
            "evidence": "child stdio uses helper threads, Windows reactor registration is socket-only, and off-worker readiness busy-polls",
        },
        {
            "id": "child_exit_readiness",
            "source": "proxima-process/src/spawn.rs",
            "line": line_with(unix_spawn, "waitpid(self.pid, &mut status, 0)", "proxima-process/src/spawn.rs"),
            "related_sources": [
                {
                    "source": "proxima-process/src/spawn_windows.rs",
                    "line": line_with(windows_spawn, "use std::process", "proxima-process/src/spawn_windows.rs"),
                }
            ],
            "evidence": "Unix wait blocks and Windows wait wraps std child lifecycle without a reactor exit event",
        },
        {
            "id": "no_std_process_feature_graph",
            "source": "proxima-process/Cargo.toml",
            "line": line_with(process_manifest, 'default = ["std"]', "proxima-process/Cargo.toml"),
            "related_sources": [
                {
                    "source": "proxima-process/Cargo.toml",
                    "line": line_with(process_manifest, 'proxima-primitives = { workspace = true, features = ["std"] }', "proxima-process/Cargo.toml"),
                }
            ],
            "evidence": "the default and normal Proxima primitive dependency features enable std",
        },
        {
            "id": "no_std_async_admission",
            "source": "proxima-primitives/src/sync/mod.rs",
            "line": line_with(sync, "mod semaphore;", "proxima-primitives/src/sync/mod.rs"),
            "related_sources": [
                {
                    "source": "proxima-primitives/src/sync/atomic_permits.rs",
                    "line": line_with(atomic_permits, "no wait queue", "proxima-primitives/src/sync/atomic_permits.rs"),
                }
            ],
            "evidence": "async Semaphore is std-gated and the alloc-tier atomic permit pool has no async waiter",
        },
        {
            "id": "no_std_prime_host_driver",
            "source": "prime/src/lib.rs",
            "line": line_with(prime_root, "pub mod os;", "prime/src/lib.rs"),
            "related_sources": [
                {
                    "source": "prime/Cargo.toml",
                    "line": line_with(prime_manifest, 'runtime-prime-reactor     = ["runtime-prime", "std"', "prime/Cargo.toml"),
                }
            ],
            "evidence": "Prime OS driver is std-gated and the reactor feature enables std",
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
    if '"std"' not in sync or "#[cfg(feature = \"std\")]" not in sync:
        raise ValueError("pinned async semaphore feature gate changed; refresh the source gap inventory")
    if "pub mod os;" not in prime_root or '"std"' not in prime_manifest:
        raise ValueError("pinned Prime host driver gate changed; refresh the source gap inventory")

    signature = "pub struct ChildSession<In, Out, Err> { /* persistent native state */ }"
    observed = gaps + [
        {"api_signature": signature},
        {"counts": {"source_backed_gaps": len(gaps), "typed_session_signatures": 1}},
        {"negative_controls": {"missing_blocking_wait_anchor_rejected": 1}},
    ]
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
    print(json.dumps(observed[-2], sort_keys=True))
    print(json.dumps(observed[-1], sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"process gap validation failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
