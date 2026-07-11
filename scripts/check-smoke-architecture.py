#!/usr/bin/env python3
"""Validate focused browser-program smoke evidence."""

import json
import sys

result = json.loads(sys.argv[1])
for key, expected in {
    "clicked": True,
    "dragged": True,
    "dropData": "architecture-drag",
    "pointer": True,
    "releaseButtons": 0,
}.items():
    if result.get(key) != expected:
        raise SystemExit(f"browser-program {key}: expected {expected!r}, got {result.get(key)!r}; {result!r}")

keys = result["keys"]
assert all(event["trusted"] for event in keys), keys
primary = [(event["type"], event["key"], event["code"], event["shift"], event["control"]) for event in keys if event["key"] not in {"Control", "Shift"}]
expected_primary = [
    ("keydown", "K", "KeyK", True, True),
    ("keyup", "K", "KeyK", True, True),
    ("keydown", "Enter", "Enter", False, False),
    ("keyup", "Enter", "Enter", False, False),
    ("keydown", "+", "Equal", True, False),
    ("keyup", "+", "Equal", True, False),
    ("keydown", "a", "KeyA", False, False),
    ("keyup", "a", "KeyA", False, False),
]
assert primary == expected_primary, (primary, expected_primary)
