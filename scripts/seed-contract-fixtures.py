#!/usr/bin/env python3
"""Seed a runtime-contract fixture corpus from the repo's JSON Schemas.

A contract check that validates zero instances proves nothing. This derives a
starting corpus from each schema so the check has something to assert on, and
so the *negative* cases exist -- an invalid fixture that the schema fails to
reject is the interesting bug, and you only find it if you write one down.

Generated fixtures are a floor, not a ceiling. Add real payloads captured from
staging; those catch drift that a schema-derived example never will.

    python3 scripts/seed-contract-fixtures.py            # dry run
    python3 scripts/seed-contract-fixtures.py --write
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

SKIP_DIRS = {".git", "node_modules", "target", ".dart_tool", "build", "vendor"}


def find_schemas(root: Path) -> list[Path]:
    """Use the checker's own discovery, so the two never disagree.

    They previously each had their own rules, and the seeder happily wrote
    fixtures for schemas the checker did not recognise. Those fixtures then
    fell through to the checker's single-schema fallback and were validated
    against an unrelated schema -- a guaranteed red build with a baffling
    message. One implementation, imported, removes the whole class of bug.
    """
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    try:
        from importlib import import_module

        checker = import_module("check-generated-contract".replace("-", "_"))
    except ImportError:
        checker = None
    if checker is None:
        # Same directory, but the module name has dashes; load it by path.
        import importlib.util

        spec = importlib.util.spec_from_file_location(
            "cgc", Path(__file__).resolve().parent / "check-generated-contract.py"
        )
        if spec is None or spec.loader is None:
            return []
        checker = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(checker)
    return [
        path
        for path in checker.discover_schemas(root)
        if checker.looks_like_schema(checker.load_json(path))
    ]


def sample_for(spec: dict[str, Any], depth: int = 0) -> Any:
    """A minimal value satisfying `spec`. Deliberately boring."""
    if depth > 6:
        return None
    if "const" in spec:
        return spec["const"]
    if "enum" in spec and spec["enum"]:
        return spec["enum"][0]
    if "default" in spec:
        return spec["default"]
    if "examples" in spec and spec["examples"]:
        return spec["examples"][0]

    # Composition keywords: take the first branch and build from that. Without
    # this, a schema that says oneOf/[object, object] has no top-level "type",
    # falls through to the string branch, and yields "x" for something that
    # must be an object.
    for key in ("allOf", "oneOf", "anyOf"):
        branches = spec.get(key)
        if isinstance(branches, list) and branches:
            merged = dict(spec)
            merged.pop(key, None)
            first = branches[0]
            if isinstance(first, dict):
                for k, v in first.items():
                    if k == "properties" and isinstance(merged.get("properties"), dict):
                        merged["properties"] = {**merged["properties"], **v}
                    elif k == "required" and isinstance(merged.get("required"), list):
                        merged["required"] = list(dict.fromkeys(merged["required"] + list(v)))
                    else:
                        merged.setdefault(k, v)
            return sample_for(merged, depth + 1)

    kind = spec.get("type")
    if isinstance(kind, list):
        kind = next((k for k in kind if k != "null"), kind[0] if kind else None)

    if kind == "object" or "properties" in spec:
        obj: dict[str, Any] = {}
        props = spec.get("properties", {})
        for name in spec.get("required", list(props)[:3]):
            obj[name] = sample_for(props.get(name, {}), depth + 1)
        return obj
    if kind == "array":
        items = spec.get("items", {})
        n = max(1, int(spec.get("minItems", 1)))
        return [sample_for(items, depth + 1) for _ in range(min(n, 2))]
    if kind == "integer":
        return int(spec.get("minimum", 1))
    if kind == "number":
        return float(spec.get("minimum", 1))
    if kind == "boolean":
        return True
    if kind == "null":
        return None
    # string, or unconstrained
    n = max(1, int(spec.get("minLength", 1)))
    fmt = spec.get("format")
    if fmt == "date-time":
        return "2026-01-01T00:00:00Z"
    if fmt == "uri":
        return "https://example.invalid/x"
    if fmt == "uuid":
        return "00000000-0000-4000-8000-000000000000"
    return "x" * n


def negatives(schema: dict[str, Any], valid: Any) -> list[tuple[str, Any]]:
    """Instances the schema MUST reject. Each targets one specific rule."""
    out: list[tuple[str, Any]] = []
    if not isinstance(valid, dict):
        return out
    props = schema.get("properties", {})

    for name in schema.get("required", []):
        if name in valid:
            broken = dict(valid)
            broken.pop(name, None)
            out.append((f"missing-required-{name}", broken))

    for name, spec in list(props.items())[:3]:
        if name not in valid:
            continue
        kind = spec.get("type")
        if isinstance(kind, list):
            kind = kind[0] if kind else None
        if kind == "string":
            out.append((f"wrong-type-{name}", {**valid, name: 12345}))
            if spec.get("minLength", 0) > 0:
                out.append((f"empty-{name}", {**valid, name: ""}))
        elif kind in ("integer", "number"):
            out.append((f"wrong-type-{name}", {**valid, name: "not-a-number"}))
        elif kind == "object":
            out.append((f"wrong-type-{name}", {**valid, name: []}))

    if schema.get("additionalProperties") is False:
        # This is how schema drift actually shows up: a peer on a newer
        # revision sends a field this one has never heard of.
        out.append(("unknown-field", {**valid, "__unexpected_field__": "drift"}))

    return out


def verify(schema: dict[str, Any], instance: Any, should_pass: bool) -> bool:
    """Only emit a fixture that actually behaves the way its folder claims.

    A "valid" fixture the schema rejects, or an "invalid" one it accepts, would
    turn the contract check red on day one for a reason that is the seeder's
    fault, not the repo's. When jsonschema is unavailable we cannot tell, so we
    emit nothing rather than guess.
    """
    try:
        import jsonschema  # type: ignore
    except ImportError:
        return False
    try:
        validator = jsonschema.validators.validator_for(schema)(schema)
        ok = next(iter(validator.iter_errors(instance)), None) is None
    except Exception:
        return False
    return ok is should_pass


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--root", type=Path, default=Path.cwd())
    ap.add_argument("--write", action="store_true", help="write files (default: dry run)")
    args = ap.parse_args()
    root = args.root.resolve()

    schemas = find_schemas(root)
    if not schemas:
        print("no JSON Schema documents found; nothing to seed")
        return 0

    vdir = root / "tests" / "generated-contract" / "valid"
    idir = root / "tests" / "generated-contract" / "invalid"
    written = 0
    for path in schemas:
        doc = json.loads(path.read_text(encoding="utf-8"))
        stem = path.name.replace(".schema.json", "").replace(".json", "")
        valid = sample_for(doc)
        if not verify(doc, valid, True):
            print(f"{path.relative_to(root)} -> SKIPPED (could not derive a valid instance)")
            continue
        bad = [(l, i) for l, i in negatives(doc, valid) if verify(doc, i, False)]
        print(f"{path.relative_to(root)} -> 1 valid + {len(bad)} invalid")
        if not args.write:
            continue
        vdir.mkdir(parents=True, exist_ok=True)
        idir.mkdir(parents=True, exist_ok=True)
        (vdir / f"{stem}.json").write_text(json.dumps(valid, indent=2) + "\n")
        written += 1
        for label, inst in bad:
            (idir / f"{stem}.{label}.json").write_text(json.dumps(inst, indent=2) + "\n")
            written += 1
    if args.write:
        print(f"wrote {written} fixture(s) under tests/generated-contract/")
    else:
        print("(dry run -- pass --write to create these)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
