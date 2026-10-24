"""Verifies every receipt of a JSON lines dump (tools/crosscheck.sh) at level signed with the
Python library datumesh, and checks its receipt hash."""
import json
import sys
from importlib.metadata import version

from datumesh import keys_from_response, verify

seen = set()
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
print(f"python datumesh {version('datumesh')}: {len(seen)} receipts verified at signed")
