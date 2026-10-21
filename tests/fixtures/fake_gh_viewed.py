#!/usr/bin/env python3
"""Only the GraphQL boundary; fixtures supply candidates, never select them."""
import json
import os
import pathlib
import re
import sys
import time

root = pathlib.Path(os.environ["FAKE_GH_VIEWED_DIR"])
args = sys.argv[1:]
assert args[:2] == ["api", "graphql"], args
assert args[2:4] == ["--hostname", "github.com"], args
fields = dict(arg.split("=", 1) for arg in args[5::2])
query = fields.pop("query")
with (root / "calls.jsonl").open("a") as log:
    log.write(json.dumps({"query": query, **fields}) + "\n")
fixture = json.loads((root / "responses.json").read_text())
if query.startswith("mutation"):
    mutation = "unmarkFileAsViewed" if "unmarkFileAsViewed" in query else "markFileAsViewed"
    time.sleep(fixture.get("delays", {}).get(mutation, 0))
    with (root / "completed.jsonl").open("a") as log:
        log.write(json.dumps(mutation) + "\n")
    response = fixture.get("mutation", {"data": {mutation: {"clientMutationId": None}}})
elif "pullRequests(" in query:
    repository = fixture["lookup"][fields["o"] + "/" + fields["n"]]
    response = {"data": {"repository": {
        key: {"nodes": repository.get(key[0], [])}
        for key in re.findall(r"([oh]\d+):pullRequests", query)
    }}}
else:
    assert "{id state}" in query, query
    number = re.search(r"pullRequest\(number:(\d+)\)", query)[1]
    node = fixture["detail"][fields["o"] + "/" + fields["n"] + "#" + number]
    response = node if node and "errors" in node else {"data": {"repository": {"pullRequest": node}}}
print(json.dumps(response))
