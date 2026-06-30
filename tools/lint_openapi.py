#!/usr/bin/env python3
"""
lint_openapi.py — minimal OpenAPI 3.1 sanity checker for lumen_api/openapi.yaml.

Stdlib only. We don't parse YAML — we scan the raw text with regex.
That's good enough for our needs: catch the cheap stuff (top-level
required keys, every operation has an operationId and at least one
response, every $ref resolves, every schema type is valid).

Stdlib-only by design. If a fuller validator is ever needed,
`pip install openapi-spec-validator` is the upgrade path. This
script catches the cheap stuff that matters at PR review time.

Usage:
    tools/lint_openapi.py lumen_api/openapi.yaml
"""
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

_VALID_TYPES = {"string", "integer", "number", "boolean", "object", "array", "null"}
_REQUIRED_TOP_LINES = [
    re.compile(r"^openapi:\s*3\.", re.MULTILINE),
    re.compile(r"^info:\s*$", re.MULTILINE),
    re.compile(r"^paths:\s*$", re.MULTILINE),
]


def lint(path: Path) -> list[str]:
    errors: list[str] = []
    text = path.read_text()

    # 1. Top-level required keys (regex on raw text — no parse).
    for pat in _REQUIRED_TOP_LINES:
        if not pat.search(text):
            errors.append(f"missing top-level key matching {pat.pattern!r}")

    # 2. Every path has at least one HTTP method with an operationId and a responses block.
    #    A path entry looks like `  /api/foo:` followed by an indented block.
    path_blocks = re.findall(r"^\s{2}(\/[\w/{}\-]+):\s*$(.*?)(?=^\s{2}\/[\w/{}\-]+:\s*$|\Z)",
                             text, flags=re.MULTILINE | re.DOTALL)
    if not path_blocks:
        errors.append("no paths found (or path indentation not 2 spaces)")
    methods_re = re.compile(r"^\s{4}(get|post|put|patch|delete):\s*$", re.MULTILINE)
    opid_re = re.compile(r"^\s{6}operationId:\s*\S+", re.MULTILINE)
    resp_re = re.compile(r"^\s{6}responses:\s*$", re.MULTILINE)
    for p, body in path_blocks:
        if not methods_re.search(body):
            errors.append(f"path {p!r}: no HTTP methods defined")
            continue
        for m in methods_re.finditer(body):
            method = m.group(1)
            # Find this method's block up to the next method at the same indent.
            start = m.end()
            next_m = methods_re.search(body, start)
            block = body[start:next_m.start() if next_m else len(body)]
            if not opid_re.search(block):
                errors.append(f"{method.upper()} {p}: missing operationId")
            if not resp_re.search(block):
                errors.append(f"{method.upper()} {p}: missing responses")

    # 3. Every $ref to a schema resolves. Walk once: track whether we're
    #    inside `components:` -> `schemas:` and collect schema names.
    defined: set[str] = set()
    in_components = False
    in_schemas = False
    for line in text.splitlines():
        if not line.strip():
            continue
        indent = len(line) - len(line.lstrip())
        stripped = line.lstrip().rstrip(":").strip()
        if indent == 0 and stripped == "components":
            in_components = True
            in_schemas = False
            continue
        if in_components and indent == 2 and stripped == "schemas":
            in_schemas = True
            continue
        # Any other indent at this level exits the relevant block.
        if in_components and indent <= 1:
            in_components = False
            in_schemas = False
        if in_schemas and indent == 4 and re.match(r"^\w+$", stripped):
            defined.add(stripped)
    refs = set(re.findall(r"\$ref:\s*['\"]?#/components/schemas/(\w+)", text))
    unresolved = refs - defined
    if unresolved:
        errors.append(f"unresolved $refs: {sorted(unresolved)}")

    # 4. Schema types are valid.
    bad_types = []
    for m in re.finditer(r"^\s+type:\s*(\S+)\s*$", text, flags=re.MULTILINE):
        if m.group(1) not in _VALID_TYPES:
            bad_types.append(m.group(1))
    if bad_types:
        errors.append(f"invalid schema types: {sorted(set(bad_types))}")
    return errors


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("spec", type=Path)
    args = p.parse_args()
    errors = lint(args.spec)
    if errors:
        for e in errors:
            print(f"  ERR  {e}", file=sys.stderr)
        print(f"{len(errors)} error(s) in {args.spec}", file=sys.stderr)
        return 1
    print(f"{args.spec}: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())