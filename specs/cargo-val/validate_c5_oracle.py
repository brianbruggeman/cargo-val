import json
from pathlib import Path


root = Path(__file__).parent
records = [json.loads(line) for line in (root / "oracles/c5.jsonl").read_text().splitlines()]
expected = ["alpha", "beta", "gamma", "delta"]
chunks = [record for record in records if record["kind"] == "chunk"]
names = [record["expected"].removeprefix("dispatch:") for record in chunks]
source_lines = [f"{name}: test\n" for name in expected]
assert len(chunks) == 4
assert names == expected
assert [record["bytes"] for record in chunks] == source_lines
assert all(record["source_state"] == "open" for record in chunks)
assert chunks[0]["bytes"] == "alpha: test\n"
assert records[-1] == {"kind": "eof", "source_state": "eof", "expected": "finish"}
assert [record["expected"] for record in records] == [
    "dispatch:alpha",
    "dispatch:beta",
    "dispatch:gamma",
    "dispatch:delta",
    "finish",
]
source = (root.parent.parent / "src/discovery_stream.rs").read_text()
assert "Vec<" not in source
print(json.dumps({"dispatches_before_eof": 1, "names_after_first_dispatch": 3, "literal_name_order": 4, "eof_after_names": 1, "production_suite_vector_tokens": 0}))
