---
name: Bug report
about: Report a reproducible problem with current behavior
title: ''
labels: bug
assignees: ''
---

## Summary

<!-- One sentence: what happened. -->

## Environment

- Commit (`git rev-parse HEAD`):
- OS / arch (`uname -a`):
- Rust (`rustc --version`):
- Config changes from example (if any):

## Steps to reproduce

1.
2.
3.

## Expected vs actual

- Expected:
- Actual:

## Evidence

<!-- Attach or reference a dated log. Do not claim "verified" without a run artifact.
     See doc/TEST-RESULTS.md and doc/LOGS.md for the evidence policy. -->

## Scope check

- [ ] This does not require restoring a removed-by-design subsystem (`AGENTS.md`).
- [ ] This does not require changing EVM/executor, ERC-20 fees, DeFi V2, `eth_*` names, tx types, or MetaMask behavior.
- [ ] This does not require a consensus/`calibri-lite` change without explicit operator approval.
