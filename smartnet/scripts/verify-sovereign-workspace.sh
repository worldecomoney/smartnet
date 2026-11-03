#!/usr/bin/env bash
# Verify docs/smartnet/sovereign-workspace-draft.toml matches root Cargo.toml workspace.members.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
python3 << 'PY'
import re
import sys
from pathlib import Path

def extract_members(text: str) -> list[str]:
    members: list[str] = []
    in_members = False
    for line in text.splitlines():
        if line.strip() == "members = [":
            in_members = True
            continue
        if in_members:
            if line.strip() == "]":
                break
            m = re.search(r'"([^"]+)"', line)
            if m:
                members.append(m.group(1).rstrip("/"))
    return members

def extract_default(text: str) -> list[str]:
    m = re.search(r"default-members\s*=\s*\[(.*?)\]", text, re.S)
    if not m:
        return []
    return [p.rstrip("/") for p in re.findall(r'"([^"]+)"', m.group(1))]

root = Path("Cargo.toml").read_text()
draft = Path("docs/smartnet/sovereign-workspace-draft.toml").read_text()
root_m = extract_members(root)
draft_m = extract_members(draft)
root_d = extract_default(root)
draft_d = extract_default(draft)
ok = True
if root_m != draft_m:
    rs, ds = set(root_m), set(draft_m)
    if rs != ds:
        ok = False
        print("MEMBERS MISMATCH")
        print("  only in Cargo.toml:", sorted(rs - ds))
        print("  only in sovereign-workspace-draft.toml:", sorted(ds - rs))
    else:
        print(f"members OK ({len(rs)} paths; order differs — acceptable)")
else:
    print(f"members OK ({len(root_m)} paths)")
if root_d != draft_d:
    ok = False
    print("DEFAULT-MEMBERS MISMATCH")
    print("  root:", root_d)
    print("  draft:", draft_d)
else:
    print(f"default-members OK ({len(root_d)} targets)")
sys.exit(0 if ok else 1)
PY
