#!/usr/bin/env python3
"""Validate real tools/list schemas independently. Requires python3-jsonschema.
Usage: python3 scripts/check-mcp-contract.py ARTIFACT_DIRECTORY
"""
import json
import pathlib
import sys
from jsonschema import Draft202012Validator

root = pathlib.Path(sys.argv[1])
catalog = json.loads((root / "mcp-contract-catalog.json").read_text())
tools = {t["name"]: t["inputSchema"] for t in catalog["result"]["tools"]}
cases = [
    ("computer_act", {"action":{"kind":"type","text":"hello"}}, True),
    ("computer_act", {"action":{"kind":"type"}}, False),
    ("computer_act", {"action":{"kind":"click"}}, False),
    ("computer_act", {"action":{"kind":"click","target":{"x":10,"y":20}}}, True),
    ("computer_act", {"action":{"kind":"type","text":"hello","target":"e1"}}, False),
    ("computer_act", {"steps":[{"action":{"kind":"key","keys":"Return","hold_ms":1}}]}, False),
    ("computer_act", {"steps":[{"action":{"kind":"key","keys":"Return"},"expect":{"kind":"url","contains":"example"}}]}, True),
    ("computer_act", {"steps":[{"action":{"kind":"key","keys":"Return"},"expect":{"kind":"url"}}]}, False),
    ("browser_act", {"action":{"kind":"type","target":"b1","text":"hello"}}, True),
    ("browser_act", {"action":{"kind":"navigate","text":"hello"}}, False),
    ("computer_files", {"op":"send","path":"a","to":{"host":"laptop","path":"/tmp/a"}}, True),
    ("computer_files", {"op":"send","path":"a"}, False),
    ("computer_files", {"op":"status"}, True),
    ("computer_files", {"op":"read","path":"a","text":"wrong"}, False),
    ("computer_wait", {"for":{"op":"op_1"},"deadline_ms":1000}, True),
    ("computer_wait", {"for":{"expect":{"kind":"url"}},"deadline_ms":1000}, False),
]
results = []
for name, schema in tools.items():
    Draft202012Validator.check_schema(schema)
    assert "Same shape as" not in json.dumps(schema), name
for name, fields, expected in cases:
    args = {"task_ref":"task_1", **fields}
    if name != "computer_wait":
        args["request_id"] = "r1"
    valid = Draft202012Validator(tools[name]).is_valid(args)
    results.append({"tool":name,"arguments":args,"expected_valid":expected,"valid":valid})
(root / "mcp-schema-validation.json").write_text(json.dumps(results, indent=2) + "\n")
failed = [r for r in results if r["valid"] != r["expected_valid"]]
assert not failed, json.dumps(failed, indent=2)
print(f"{len(tools)} standalone schemas; {len(results)} valid/invalid contract cases passed")
