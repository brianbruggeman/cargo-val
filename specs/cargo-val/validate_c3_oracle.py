from __future__ import annotations

import json
from pathlib import Path


SPEC = Path(__file__).resolve().parent
EVENTS = SPEC / "oracles" / "c3.jsonl"
EXPECTED = SPEC / "oracles" / "c3.expected.jsonl"
CARGO = SPEC / "evidence" / "c2" / "cargo-all-targets.jsonl"


def load(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text().splitlines()]


def replay(events: list[dict]) -> list[dict]:
    pending = ""
    paths: list[str] = []
    snapshots: list[dict] = []
    for index, event in enumerate(events):
        if event["kind"] == "chunk":
            pending += event["bytes"]
            while "\n" in pending:
                line, pending = pending.split("\n", 1)
                record = json.loads(line)
                if record["reason"] == "compiler-artifact" and record["profile"]["test"]:
                    paths.append(record["executable"])
        elif event["kind"] == "eof":
            if pending:
                raise ValueError("unterminated Cargo JSON line at EOF")
        else:
            raise ValueError("unknown oracle event")
        snapshots.append(
            {
                "after_event": index,
                "producer_open": event["producer_open"],
                "paths": paths.copy(),
                "partial_lines": int(bool(pending)),
            }
        )
    return snapshots


def main() -> None:
    events = load(EVENTS)
    expected = load(EXPECTED)
    observed = replay(events)
    if observed != expected:
        raise ValueError(f"literal snapshots disagree: {observed!r}")
    if [row["kind"] for row in events] != ["chunk", "chunk", "chunk", "eof"]:
        raise ValueError("literal event phases changed")
    if expected[1]["paths"] != ["/fixture/first"] or not expected[1]["producer_open"]:
        raise ValueError("held-open first emission missing")

    cargo_artifacts = [
        row for row in load(CARGO)
        if row.get("reason") == "compiler-artifact" and row.get("profile", {}).get("test")
    ]
    literal_lines = "".join(event["bytes"] for event in events if event["kind"] == "chunk")
    literal_records = [json.loads(line) for line in literal_lines.splitlines()]
    cargo_shapes = [
        (row["target"]["kind"], row["target"]["test"], row["profile"]["test"], bool(row["executable"]))
        for row in cargo_artifacts
    ]
    literal_shapes = [
        (row["target"]["kind"], row["target"]["test"], row["profile"]["test"], bool(row["executable"]))
        for row in literal_records
    ]
    if cargo_shapes != literal_shapes or len(cargo_artifacts) != 2:
        raise ValueError("literal artifact shapes disagree with Cargo's retained output")

    reversed_output = [expected[3]["paths"][1], expected[3]["paths"][0]]
    reversed_rejected = int(reversed_output != expected[3]["paths"])
    no_first_newline = [dict(row) for row in events]
    no_first_newline[1]["bytes"] = no_first_newline[1]["bytes"].replace("}\n{", "}{", 1)
    missing_newline_rejected = int(replay(no_first_newline[:2])[1]["paths"] != expected[1]["paths"])
    if reversed_rejected != 1 or missing_newline_rejected != 1:
        raise ValueError("negative controls did not reject")
    print(json.dumps({
        "literal_ordered_executables": len(expected[3]["paths"]),
        "held_open_first_emission_checkpoints": int(bool(expected[1]["paths"]) and expected[1]["producer_open"]),
        "retained_cargo_test_artifact_controls": len(cargo_artifacts),
        "reversed_order_rejections": reversed_rejected,
        "missing_newline_rejections": missing_newline_rejected,
    }, sort_keys=True))


if __name__ == "__main__":
    main()
