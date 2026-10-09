from __future__ import annotations

import json
import os
from pathlib import Path
import re
import sys


ROOT = Path(
    os.environ.get(
        "CONFLAGURATION_ROOT",
        str(Path(__file__).resolve().parents[4] / "mine" / "conflaguration"),
    )
)
LIB = ROOT / "src" / "lib.rs"
MANIFEST = ROOT / "Cargo.toml"
DERIVE = ROOT / "crates" / "conflaguration-derive" / "src" / "lib.rs"
ORACLE = Path(__file__).resolve().parent / "oracles" / "f0.jsonl"
SIGNATURE = (
    "fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) "
    "-> Result<Self>;"
)


def source_line(path: Path, pattern: str) -> int:
    for line_number, line in enumerate(path.read_text().splitlines(), start=1):
        if re.search(pattern, line):
            return line_number
    raise ValueError(f"missing source anchor {pattern!r} in {path}")


def main() -> int:
    library = LIB.read_text()
    manifest = MANIFEST.read_text()
    derive = DERIVE.read_text()

    gaps = [
        {
            "id": "no_std_root",
            "source": "src/lib.rs",
            "line": source_line(LIB, r"^mod builder;"),
            "evidence": "crate root has no no_std attribute and includes std-bound modules",
        },
        {
            "id": "borrowed_environment_source",
            "source": "src/lib.rs",
            "line": source_line(LIB, r"pub trait Settings: Sized"),
            "evidence": "Settings exposes from_env but no borrowed-pair entry point",
        },
        {
            "id": "environment_lookup",
            "source": "src/lib.rs",
            "line": source_line(LIB, r"std::env::var\(key\)"),
            "evidence": "runtime environment resolution calls std::env directly",
        },
        {
            "id": "std_bound_error_and_display",
            "source": "src/lib.rs",
            "line": source_line(LIB, r"impl std::fmt::Display for ValidationMessage"),
            "evidence": "public error/display surface names std types",
        },
        {
            "id": "derive_generated_std_paths",
            "source": "crates/conflaguration-derive/src/lib.rs",
            "line": source_line(DERIVE, r"::std::env::var\(__k\)"),
            "evidence": "Settings derive emits std environment access",
        },
    ]

    dependencies = re.findall(
        r"(?m)^([A-Za-z0-9_-]+)\s*=\s*", manifest.split("[dependencies]", 1)[1].split("[", 1)[0]
    )
    tracked_dependencies = {
        "environs",
        "thiserror",
        "conflaguration-derive",
        "serde",
        "toml",
        "serde_json",
        "serde_yaml",
        "serde-value",
    }
    unbound_dependencies = sorted(set(dependencies) - tracked_dependencies)
    observed = gaps + [{"api_signature": SIGNATURE}]
    expected = [json.loads(line) for line in ORACLE.read_text().splitlines() if line.strip()]
    if observed != expected or len(gaps) != 5 or unbound_dependencies:
        raise ValueError(
            f"expected 5 gaps, 1 API signature, 0 unbound dependencies; "
            f"observed {len(gaps)}, {sum('api_signature' in row for row in observed)}, "
            f"{len(unbound_dependencies)}"
        )

    for gap in gaps:
        print(json.dumps(gap, sort_keys=True))
    print(json.dumps({"api_signature": SIGNATURE}, sort_keys=True))
    print(
        json.dumps(
            {
                "counts": {
                    "source_backed_gaps": len(gaps),
                    "borrowed_pair_signatures": 1,
                    "unbound_runtime_dependencies": len(unbound_dependencies),
                },
                "dependencies_reviewed": sorted(dependencies),
                "unbound_dependencies": unbound_dependencies,
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as error:
        print(f"conflag gap validation failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
