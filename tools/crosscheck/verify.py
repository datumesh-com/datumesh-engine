"""Verifies every receipt of a JSON lines dump (tools/crosscheck.sh) at level signed with the
Python library datumesh, checks its receipt hash, and checks that the manifest given for each
slice entry among its inputs is one the library accepts with that entry."""
import json
import sys
from importlib.metadata import version

from datumesh import keys_from_response, slice_entry, verify

seen = set()
manifests = 0
for n, line in enumerate(open(sys.argv[1], encoding="utf-8"), 1):
    o = json.loads(line)
    if o["hash"] in seen:
        continue
    seen.add(o["hash"])
    keys = [keys_from_response(host, 200, doc) for host, doc in sorted(o["keys"].items())]
    r = verify(o["receipt"], keys)
    if not (r.ok and r.level == "signed" and r.hash == o["hash"]):
        print(f"line {n}: ok={r.ok} code={r.code} level={r.level} hash={r.hash} want {o['hash']}: {r.detail}")
        sys.exit(1)
    inputs = json.loads(o["receipt"])["inputs"]
    entries = [e for e in inputs if e.startswith("slice:")]
    if sorted(entries) != sorted(o.get("manifests", {}).keys()):
        print(f"line {n}: the manifests given are not those of the slice entries")
        sys.exit(1)
    for entry, text in o.get("manifests", {}).items():
        got = slice_entry(json.loads(text))
        if got != entry:
            print(f"line {n}: manifest of {entry} gives entry {got}")
            sys.exit(1)
        manifests += 1
print(f"python datumesh {version('datumesh')}: {len(seen)} receipts verified at signed, {manifests} slice-entry manifests checked")
