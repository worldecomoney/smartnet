#!/usr/bin/env bash
# Verify every workspace member/default-member path in Cargo.toml exists.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
python3 << 'PY'
import re
import sys
from pathlib import Path

def extract_paths(text: str, key: str) -> list[str]:
    m = re.search(rf"^{key}\s*=\s*\[(.*?)\]", text, re.S | re.M)
    if not m:
        return []
    return [p.rstrip("/") for p in re.findall(r'"([^"]+)"', m.group(1))]

root = Path("Cargo.toml").read_text()
paths = {
    "members": extract_paths(root, "members"),
    "default-members": extract_paths(root, "default-members"),
}
ok = True
for label, entries in paths.items():
    missing = [p for p in entries if not Path(p).is_dir()]
    if missing:
        ok = False
        print(f"{label}: missing path(s): {', '.join(missing)}")
    else:
        print(f"{label} OK ({len(entries)} paths)")
sys.exit(0 if ok else 1)
PY
