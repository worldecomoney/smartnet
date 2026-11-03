#!/usr/bin/env bash
# Phase 4 / T0-GATE helper — read-only checks + optional empty seal.
# Usage:
#   export SMARTNET_CONFIG=~/.smartnet/node/config.toml   # optional (node config)
#   export SMARTNET_RPC=http://<HTTP_ADDR>:<HTTP_PORT>    # required — from the node config
#   export ERC20_CONTRACT=0x…                             # required for the ERC20 probes
#   export VALIDATOR_CONFIG=~/.smartnet/validator/config.toml  # optional
#   ./scripts/phase4-runtime-gate.sh check
#   ./scripts/phase4-runtime-gate.sh run-once-empty
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${REPO}/bin"
CONFIG="${SMARTNET_CONFIG:-${HOME}/.smartnet/node/config.toml}"
# No built-in endpoint and no built-in contract: both are operator data.
RPC="${SMARTNET_RPC:?set SMARTNET_RPC (HTTP_ADDR/HTTP_PORT from $CONFIG)}"
ERC20="${ERC20_CONTRACT:?set ERC20_CONTRACT}"

rpc() {
  curl -s -m 15 -X POST "$RPC" -H 'Content-Type: application/json' -d "$1"
}

cmd="${1:-check}"

case "$cmd" in
  check)
    echo "chainId:"; rpc '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'
    echo "blockNumber:"; rpc '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}'
    echo "ERC20 code (first 80 chars):"; rpc "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getCode\",\"params\":[\"$ERC20\",\"latest\"]}" | head -c 80
    echo "..."
    echo "ERC20 totalSupply:"; rpc "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$ERC20\",\"data\":\"0x18160ddd\"},\"latest\"]}"
    ;;
  run-once-empty)
    echo "head before:"; rpc '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}'
    # Config-driven: NODE_URL / COINBASE / ALLOW_EMPTY come from the validator config
    # (CLI flags for them no longer exist).
    if [ -n "${VALIDATOR_CONFIG:-}" ]; then
      "$BIN/smartnet-validator" --config "$VALIDATOR_CONFIG"
    else
      "$BIN/smartnet-validator"
    fi
    echo "head after:"; rpc '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}'
    ;;
  *)
    echo "usage: $0 {check|run-once-empty}" >&2
    exit 1
    ;;
esac
