#!/usr/bin/env python3
"""Validate executable human/JSON/JSONL output contracts."""
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])

def structured(name, kind):
    pretty = (root / f"{name}.json").read_text()
    compact = (root / f"{name}.jsonl").read_text()
    assert "\n" in pretty.strip(), (name, pretty)
    assert compact.count("\n") == 1 and compact.endswith("\n"), (name, compact)
    assert compact.strip() == json.dumps(json.loads(compact), separators=(",", ":")), name
    left, right = json.loads(pretty), json.loads(compact)
    if kind == "artifact":
        assert {key: value for key, value in left.items() if key != "path"} == {
            key: value for key, value in right.items() if key != "path"
        }, (name, left, right)
    else:
        assert left == right, (name, left, right)
    assert left["schemaVersion"] == 1 and left["kind"] == kind, left
    human = (root / f"{name}.human").read_text()
    if name != "cookies":
        assert human.strip(), name
    else:
        assert human == "", human
    return left, right

for name, kind in [
    ("status", "status"), ("list", "list"), ("open", "open"),
    ("pages", "pages"), ("cookies", "cookies"), ("viewport", "viewport"),
]:
    structured(name, kind)

for name, media_type, json_path, jsonl_path in [
    ("screenshot", "image/png", root / "shot-json.png", root / "shot-jsonl.png"),
    ("pdf", "application/pdf", root / "page-json.pdf", root / "page-jsonl.pdf"),
    ("download", "text/plain", root / "download-json.txt", root / "download-jsonl.txt"),
]:
    value, value_jsonl = structured(name, "artifact")
    assert value["type"] == media_type and pathlib.Path(value["path"]).stat().st_size == value["bytes"] > 0, value
    assert pathlib.Path(value_jsonl["path"]).stat().st_size == value_jsonl["bytes"] > 0, value_jsonl
    assert json_path.is_file() and jsonl_path.is_file()
    assert value.get("instance") and value.get("target") and value.get("url"), value

assert "rdny-smoke-human" in (root / "logs.human").read_text()
for format_name in ("json", "jsonl"):
    text = (root / f"logs.{format_name}").read_text()
    decoder = json.JSONDecoder()
    records = []
    offset = 0
    while offset < len(text):
        while offset < len(text) and text[offset].isspace(): offset += 1
        if offset < len(text):
            value, offset = decoder.raw_decode(text, offset)
            records.append(value)
    record = next(item for item in records if f"rdny-smoke-{format_name}" in item["message"])
    assert record["schemaVersion"] == 1 and record["kind"] == "log", record
    assert record["instance"] and record["target"] and record["cdpSession"], record
    assert record["target"] != record["cdpSession"], record
    if format_name == "jsonl":
        assert all(line == json.dumps(json.loads(line), separators=(",", ":")) for line in text.splitlines()), text
