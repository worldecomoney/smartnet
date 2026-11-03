## smartnet-node-builder

A declarative way to configure a SmartNet ethereum node.

**SmartNet fork:** Launches HTTP/WS RPC only (no IPC). Engine methods are served on the
engine/auth port only — the wallet-port engine merge (`RPC_UNIFIED_VALIDATOR_JSONRPC`) was
removed 2026-10-28. See the repo [README](../../../../README.md).

### Examples

The [examples](../../../examples) folder contains various examples of how to use the builder to configure a SmartNet node.