#!/usr/bin/env python3
"""Validate deterministic large streaming-download bytes."""
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
expected = int(sys.argv[2])
data = path.read_bytes()
assert len(data) == expected, (path, len(data), expected)
for index in (0, 1, 1024, 8 * 1024 * 1024, expected - 1):
    assert data[index] == index % 251, (path, index, data[index], index % 251)
