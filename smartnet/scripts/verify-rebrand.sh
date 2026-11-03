#!/usr/bin/env bash
# R11 gate (RETH.md wave R11): fail if any non-allowlisted `reth` reference remains.
#
# Scan: whole repo (byte text files), case-insensitive, word-anchored:
#   (^|[^[:alnum:]_])reth([^[:alnum:]_]|$|[-_:])
#   -> catches `reth`, `Reth`, `RETH`, `reth-*`, `reth_*`, `reth:*`,
#      but NOT words merely containing the letters (e.g. `MoreThan`).
#
# Allowlist (RETH.md §4 + documented reasons):
#   dirs   : target/ .git/ .ref/ vendor/ doc/ark/ doc/tmp/  (build, VCS, upstream
#            snapshot, historical wave records)
#   files  : *.md excluded wholesale (operator ruling 2026-10-28: docs may say "reth")
#            LICENSE                         (upstream copyright line)
#            root.md                         (pinned upstream commit links, §4.2)
#            phase*-rename-crates.py         (one-shot history tools, banner'd R10)
#            verify-rebrand.sh               (this gate)
#            reth.rs/run …                   (upstream docs URL)
#            reth.toml                       (legacy config filename, read at runtime)
#            SMART-NET.md RETH.md            (the plan document's own filename, renamed)
#            SCALEUP.md README.md               (operator-authored: Gravity-Reth research + provenance line)
#            Reth Contributors               (LICENSE attribution text)
#            derived from Reth               (node/builder provenance note, §3.I)
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

PATTERN='(^|[^[:alnum:]_])reth([^[:alnum:]_]|$|[-_:])'

# Operator ruling 2026-10-28: .md files MAY contain the word "reth" (mixed-provenance
# docs, multiple authors) — the word-anchored sweep is code/config/text only.
hits="$(grep -rInE -i "$PATTERN" . \
  --exclude-dir=target --exclude-dir=.git --exclude-dir=.ref --exclude-dir=vendor \
  --exclude-dir=ark --exclude-dir=tmp \
  --exclude='*.md' \
  --exclude=LICENSE \
  --exclude='phase*-rename-crates.py' --exclude=verify-rebrand.sh \
  2>/dev/null \
  | grep -vE 'reth\.rs/run|reth\.toml|RETH\.md|Reth Contributors|derived from Reth')"

if [[ -n "$hits" ]]; then
  echo "FAIL: non-allowlisted 'reth' references found:"
  echo "$hits" | sed 's|^\./||'
  echo "---"
  echo "count: $(printf '%s\n' "$hits" | wc -l)"
  exit 1
fi

# Second sweep (operator directive): CODE-ONLY, case-insensitive SUBSTRING — catches
# partial/glued usage the word-anchored check cannot (smartnetreth, RethFoo in fn names,
# vars, consts, crate names, filenames). .md files are excluded by rule; the same
# functional/provenance allowlist and `MoreThan` (a letter-coincidence: Mo-reTh-an, not a
# reth reference) apply. Filenames are covered by the include pattern itself.
partial="$(grep -rIn -i 'reth' . \
  --include='*.rs' --include='*.toml' --include='*.sh' --include='*.py' \
  --include='*.yaml' --include='*.yml' --include='*.lock' --include='*.json' \
  --exclude-dir=target --exclude-dir=.git --exclude-dir=.ref --exclude-dir=vendor \
  --exclude-dir=ark --exclude-dir=tmp \
  --exclude='phase*-rename-crates.py' --exclude=verify-rebrand.sh \
  2>/dev/null \
  | grep -vE 'reth\.rs/run|reth\.toml|derived from Reth|MoreThan')"

if [[ -n "$partial" ]]; then
  echo "FAIL: partial/in-word 'reth' usage in code:"
  echo "$partial" | sed 's|^\./||'
  echo "---"
  echo "count: $(printf '%s\n' "$partial" | wc -l)"
  exit 1
fi

echo "OK: 0 non-allowlisted 'reth' references — code-only substring + word-anchored sweeps clean (allowlist: RETH.md §4 + gate header)"
