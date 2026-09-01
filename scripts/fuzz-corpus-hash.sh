#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="$ROOT/fuzz/corpus-manifest.json"
EVIDENCE="$ROOT/fuzz/corpus-evidence.json"

if [[ ! -f "$MANIFEST" ]]; then
  echo "missing $MANIFEST" >&2
  exit 1
fi

python3 - "$ROOT" "$MANIFEST" "$EVIDENCE" <<'PY'
import hashlib, json, os, sys
from pathlib import Path

root = Path(sys.argv[1])
manifest_path = Path(sys.argv[2])
evidence_path = Path(sys.argv[3])
manifest = json.loads(manifest_path.read_text())
targets = ["rfb_handshake", "rfb_session", "rfb_zrle", "rfb_tight", "rfb_hextile"]
if manifest.get("targets") != targets:
    raise SystemExit("manifest targets drifted from the canonical five")

by_file = {seed["file"]: seed for seed in manifest["seeds"]}
corpus = root / "fuzz" / "corpus"
seen = set()
updated = []
evidence = {"targets": {}}
if evidence_path.exists():
    evidence = json.loads(evidence_path.read_text())

for target in targets:
    directory = corpus / target
    if not directory.is_dir():
        raise SystemExit(f"missing corpus directory {target}")
    files = sorted(path.name for path in directory.iterdir() if path.is_file())
    evidence.setdefault("targets", {}).setdefault(target, {})
    evidence["targets"][target]["final_filenames"] = files
    evidence["targets"][target]["final_count"] = len(files)
    evidence["targets"][target]["files"] = []
    for name in files:
        relative = f"corpus/{target}/{name}"
        seen.add(relative)
        data = (directory / name).read_bytes()
        digest = hashlib.sha256(data).hexdigest()
        seed = by_file.get(relative)
        if seed is None:
            raise SystemExit(f"orphan corpus file {relative} has no category metadata")
        seed["sha256"] = digest
        seed["length"] = len(data)
        updated.append(seed)
        evidence["targets"][target]["files"].append(
            {
                "file": name,
                "sha256": digest,
                "length": len(data),
                "category": seed["category"],
            }
        )

missing = set(by_file) - seen
if missing:
    raise SystemExit(f"manifest lists missing files: {sorted(missing)}")

manifest["seeds"] = updated
manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
evidence_path.write_text(json.dumps(evidence, indent=2) + "\n")
print(f"updated {len(updated)} corpus hashes")
PY
