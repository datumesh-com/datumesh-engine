// Verifies every receipt of a JSON lines dump (tools/crosscheck.sh) at level signed with the
// JavaScript library @datumesh/verify, checks its receipt hash, and checks that the manifest
// given for each slice entry among its inputs is one the library accepts with that entry.
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { verify, keysFromResponse, sliceEntry } from '@datumesh/verify';

const pkg = createRequire(import.meta.url)('@datumesh/verify/package.json');
const seen = new Set();
let manifests = 0;
const lines = readFileSync(process.argv[2], 'utf8').split('\n').filter((l) => l.length > 0);
lines.forEach((line, i) => {
  const o = JSON.parse(line);
  if (seen.has(o.hash)) return;
  seen.add(o.hash);
  const keys = Object.keys(o.keys).sort().map((host) => keysFromResponse(host, 200, o.keys[host]));
  const r = verify(o.receipt, keys);
  if (!(r.ok && r.level === 'signed' && r.hash === o.hash)) {
    console.log(`line ${i + 1}: ok=${r.ok} code=${r.code} level=${r.level} hash=${r.hash} want ${o.hash}: ${r.detail}`);
    process.exit(1);
  }
  const entries = JSON.parse(o.receipt).inputs.filter((e) => e.startsWith('slice:')).sort();
  const given = Object.keys(o.manifests || {}).sort();
  if (JSON.stringify(entries) !== JSON.stringify(given)) {
    console.log(`line ${i + 1}: the manifests given are not those of the slice entries`);
    process.exit(1);
  }
  for (const [entry, text] of Object.entries(o.manifests || {})) {
    const got = sliceEntry(JSON.parse(text));
    if (got !== entry) {
      console.log(`line ${i + 1}: manifest of ${entry} gives entry ${got}`);
      process.exit(1);
    }
    manifests += 1;
  }
});
console.log(`javascript @datumesh/verify ${pkg.version}: ${seen.size} receipts verified at signed, ${manifests} slice-entry manifests checked`);
