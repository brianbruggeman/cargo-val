from __future__ import annotations

import json
from pathlib import Path
import subprocess
import tempfile


SPEC = Path(__file__).resolve().parent
ORACLE = SPEC / "oracles" / "c4.jsonl"
CONTROL_SOURCE = SPEC / "oracles" / "c4-libtest-control.rs"


def decode_name(line: str) -> str | None:
    suffix = ": test"
    return line[:-len(suffix)] if line.endswith(suffix) and len(line) > len(suffix) else None


def main() -> None:
    rows = [json.loads(line) for line in ORACLE.read_text().splitlines()]
    if [row["kind"] for row in rows] != ["libtest_name", "summary", "mutated_suffix"]:
        raise ValueError("literal line roles changed")
    if [decode_name(row["line"]) for row in rows] != [row["expected_name"] for row in rows]:
        raise ValueError("literal name outcomes disagree with independent suffix decoder")

    with tempfile.TemporaryDirectory() as directory:
        binary = Path(directory) / "c4-libtest-control"
        compile_result = subprocess.run(
            ["rustc", "--edition=2024", "--test", str(CONTROL_SOURCE), "-o", str(binary)],
            capture_output=True,
            text=True,
            timeout=30,
            check=True,
        )
        listing = subprocess.run(
            [str(binary), "--list"],
            capture_output=True,
            text=True,
            timeout=10,
            check=True,
        )
    if compile_result.stdout or compile_result.stderr or listing.stderr:
        raise ValueError("independent libtest control emitted unexpected diagnostics")
    control_lines = [line for line in listing.stdout.splitlines() if line]
    if control_lines != [rows[0]["line"], rows[1]["line"]]:
        raise ValueError(f"libtest listing differs from literal oracle: {control_lines!r}")
    mutated_name = decode_name(rows[2]["line"])
    if mutated_name is not None:
        raise ValueError("changed suffix still selects a test")

    literal_names = [row["expected_name"] for row in rows if row["kind"] == "libtest_name"]
    literal_summaries = [row for row in rows if row["kind"] == "summary"]
    live_names = [line for line in control_lines if decode_name(line) is not None]
    live_summaries = [line for line in control_lines if decode_name(line) is None]
    print(json.dumps({
        "literal_test_names": len(literal_names),
        "literal_summary_lines_ignored": len(literal_summaries),
        "mutated_suffixes_rejected": int(mutated_name is None),
        "live_libtest_name_controls": len(live_names),
        "live_libtest_summary_controls": len(live_summaries),
    }, sort_keys=True))


if __name__ == "__main__":
    main()
