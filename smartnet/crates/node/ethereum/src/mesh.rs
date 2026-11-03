//! Mesh service — closed-list all-to-all binary `WebSocket` gossip.
//!
//! Three background tasks, all started unconditionally (the mesh is always on — the
//! operator supplies `MESH_PORT`/`MESH_ADDR`, and a bind failure is a boot failure;
//! injection itself is **in-process**, no loopback socket, so the RPC is optional):
//!
//! 1. **`/mesh` `WebSocket` listener** — [`serve`] on `MESH_ADDR:MESH_PORT` (default `0.0.0.0`).
//!    Every connection opens with the signed `HELLO` identity handshake (allowlist +
//!    nonce replay cache) — an unauthenticated peer never reaches the protocol loop.
//!    Inbound binary frames are decoded by [`smartnet_mesh::protocol`]; a tx/block
//!    announce (`RUMOR_TX_PING` / `RUMOR_BLOCKS_PING`) is answered with `PONG_OK` or a
//!    counter-request, and delivered data is check-and-marked in the shared [`SeenCache`]
//!    *before* local inject via [`NodeInjector`] (`EthApi::send_raw_transaction` / engine
//!    `new_payload` — the node's own validated RPC semantics, called in-process with
//!    no socket). The truthful `PING_OK` lands only after the inject completes. Sync
//!    status/fetch (`SYNC_BLOCK_*`) serve the tip and block RLP from the same injector.
//!    `PING`/`PONG_OK` is answered on the persistent link (keepalive, not a sweep).
//! 2. **block relay** — canon-state stream → canonical block hash + full-block RLP (seal-path
//!    `Encodable`, the same encoding `smartnet_newPayload` consumes) → mark/store → spread, awaited
//!    block-by-block so per-peer `WebSocket` order is block order. Genesis is never spread.
//! 3. **tx relay** — pool event stream (`Pending`/`Queued`) → EIP-2718 bytes → mark/store → spread,
//!    detached (transaction order at the receiver is the pool's problem, not the mesh's).
//! There is no liveness sweep: each peer keeps one persistent `WebSocket` link
//! ([`Spreader`] owns the [`LinkPool`]) with `PING`/`PONG` keepalive and bounded-reconnect
//! backoff — broken links are detected and restored by the link itself.
//!
//! [`LinkPool`]: smartnet_mesh::link::LinkPool
//!
//! Spread is fire-and-forget to `active && valid` peers of `MESH_PEERS_FILE` (default
//! `~/.smartnet/mainnet.jsonl`), re-read on every spread. Self-identity is the listen port:
//! an entry whose `port_range` equals our `MESH_PORT` is skipped as self, so every node needs
//! its own distinct port. The peer URL is `ws://ip:port/mesh`.
//!
//! Failure policy: the mesh must never be off. A missing `MESH_PORT`/`MESH_ADDR`, a port
//! below 1024, or a bind failure is fatal — the boot aborts with a clear error rather than
//! running a node that silently has no mesh. An empty/absent peer file is fine: the node
//! just has no one to gossip with yet.

use alloy_eips::eip2718::Encodable2718;
use alloy_network::Ethereum;
use alloy_primitives::Bytes;
use alloy_rpc_types_engine::ExecutionData;
use alloy_rpc_types_eth::{BlockNumberOrTag, BlockTransactions};
use async_trait::async_trait;
use futures::StreamExt;
use smartnet_ethereum_primitives::{Block as ConsensusBlock, EthPrimitives};
use smartnet_transaction_pool::{FullTransactionEvent, TransactionPool};
use smartnet_chain_state::CanonStateSubscriptions;
use smartnet_engine_primitives::ConsensusEngineHandle;
use smartnet_engine_primitives::ethereum::execution_data_from_block_rlp;
use smartnet_mesh::{
    identity::Identity,
    inject::{is_benign_duplicate, Injector},
    peers::default_peers_file,
    protocol::Limits,
    rumor::Rumor,
    seen::SeenCache,
    spread::{SpreadConfig, Spreader},
    store::ObjectCache,
    sync_gate::SyncGate,
    ws::{self, serve, MeshServer, ServerConfig},
};
use smartnet_node_api::{AddOnsContext, FullNodeComponents};
use smartnet_node_builder::node::NodeTypes;
use smartnet_payload_primitives::PayloadTypes;
use smartnet_rpc_eth_api::{EthApiServer, FullEthApiServer};
use smartnet_tasks::TaskExecutor;
use smartnet_validator::{config::load_validator_config, keystore_util::load_identity_from_config};
use std::{
    fmt,
    net::{IpAddr, Ipv4Addr},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::net::TcpListener;
use tracing::{debug, error};

/// Single source of truth for "should the mesh run?" — mirrors `sync_gate`: everything
/// the operator has to provide is checked in one place so a misconfiguration can never
/// start half a mesh.
///
/// The mesh is always on: `MESH_PORT` and `MESH_ADDR` are required (the CLI enforces this
/// too) and the port must be a reachable one (>= 1024). The RPC is the optional half of
/// the pair — it can be off while the mesh stays up (injection is in-process, no socket).
/// A failure here is fatal: the mesh must never silently be off.
fn mesh_gate(mesh_port: Option<u16>) -> Result<u16, String> {
    let Some(port) = mesh_port else {
        return Err("MESH_PORT is not set — the mesh is always on".to_string());
    };
    if port < 1024 {
        return Err(format!(
            "MESH_PORT ({port}) is below 1024 — unprivileged ports only (the mesh is always \
             on; pick a port >= 1024)"
        ));
    }
    Ok(port)
}

/// In-process mesh injection: raw txs through the live [`EthApi`] (the exact handler behind
/// `eth_sendRawTransaction`), blocks through the consensus engine (the exact handler behind
/// `smartnet_newPayload`). No HTTP client, URL, or loopback socket exists in this path —
/// the same object identity the merged validator already uses in-process.
///
/// [`EthApi`]: FullEthApiServer
pub(crate) struct NodeInjector<A, P>
where
    P: PayloadTypes,
{
    eth_api: A,
    engine: ConsensusEngineHandle<P>,
}

impl<A, P: PayloadTypes> fmt::Debug for NodeInjector<A, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeInjector").finish_non_exhaustive()
    }
}

impl<A, P: PayloadTypes> NodeInjector<A, P> {
    /// Builds the in-process injector from the node's live `EthApi` and engine handle.
    pub(crate) const fn new(eth_api: A, engine: ConsensusEngineHandle<P>) -> Self {
        Self { eth_api, engine }
    }
}

#[async_trait]
impl<A, P> Injector for NodeInjector<A, P>
where
    A: FullEthApiServer<NetworkTypes = Ethereum, Primitives = EthPrimitives>
        + Send
        + Sync
        + 'static,
    P: PayloadTypes<ExecutionData = ExecutionData> + Send + Sync + 'static,
{
    async fn send_raw_transaction(&self, raw: &Bytes) -> Result<(), String> {
        match EthApiServer::send_raw_transaction(&self.eth_api, raw.clone()).await {
            Ok(_) => Ok(()),
            Err(err) => {
                let message = err.message().to_string();
                if is_benign_duplicate(&message) {
                    debug!(
                        target: "smartnet::mesh",
                        %message,
                        "mesh tx already present locally"
                    );
                    Ok(())
                } else {
                    Err(format!("[{}] {message}", err.code()))
                }
            }
        }
    }

    async fn new_payload(&self, block_rlp: &Bytes) -> Result<(), String> {
        let block: smartnet_ethereum_primitives::Block =
            alloy_rlp::Decodable::decode(&mut block_rlp.as_ref())
                .map_err(|e| format!("decode sealed block RLP: {e}"))?;
        let (status, _timings) = self
            .engine
            .new_payload(execution_data_from_block_rlp(block), true, true)
            .await
            .map_err(|e| format!("engine new_payload: {e}"))?;
        // Any payload status (VALID/SYNCING/INVALID) is a decision already made — v1 has
        // no retries; only transport/engine failure above is an inject error.
        debug!(
            target: "smartnet::mesh",
            status = ?status.status,
            "mesh block injected via engine"
        );
        Ok(())
    }

    async fn last_block_nr(&self) -> Result<u64, String> {
        let tip = EthApiServer::block_number(&self.eth_api)
            .map_err(|err| format!("eth_blockNumber: {err}"))?;
        u64::try_from(tip).map_err(|_| format!("tip {tip} exceeds u64"))
    }

    async fn block_rlp(&self, block_nr: u64) -> Result<Option<Bytes>, String> {
        let tag = BlockNumberOrTag::Number(block_nr);
        let block = EthApiServer::block_by_number(&self.eth_api, tag, true)
            .await
            .map_err(|err| format!("eth_getBlockByNumber: {err}"))?;
        let Some(block) = block else { return Ok(None) };
        // RPC block (full transactions) → consensus block → seal-path RLP — the same
        // encoding `smartnet_newPayload` decodes (mirrors `smartnet-engine-rpc-sync`'s
        // `into_engine_block`, which lives in its own crate — mesh code never calls it).
        let transactions = match block.transactions {
            BlockTransactions::Full(txs) => txs
                .into_iter()
                .map(|tx| tx.into_inner().into())
                .collect::<Vec<_>>(),
            _ => return Err("block transactions are hashes, not full".to_string()),
        };
        let consensus: ConsensusBlock = alloy_consensus::Block {
            header: block.header.into_consensus(),
            body: alloy_consensus::BlockBody { transactions, ommers: vec![], withdrawals: block.withdrawals },
        };
        let mut rlp = Vec::new();
        alloy_rlp::Encodable::encode(&consensus, &mut rlp);
        Ok(Some(rlp.into()))
    }
}

/// Loads the mesh identity from the operator's validator config — the same keystore the
/// validator uses (`KEY_PATH`/`KEY_FILE`/`PASSWORD_FILE`), decrypted once at boot,
/// memory-only. Any failure is fatal: the mesh must never boot with a missing or guessed
/// identity (peers could not authenticate us).
fn load_mesh_identity(config_path: Option<&Path>, chain_id: u64) -> Result<Identity, String> {
    let path = config_path
        .ok_or_else(|| "validator config path missing — mesh identity keystore cannot be loaded".to_string())?;
    let config =
        load_validator_config(path).map_err(|err| format!("validator config: {err:#}"))?;
    let password_file = config
        .password_file
        .as_deref()
        .ok_or_else(|| "PASSWORD_FILE is not set — mesh identity keystore cannot be loaded".to_string())?;
    let key = load_identity_from_config(&config.key_dir, &config.key_file, password_file)
        .map_err(|err| format!("mesh identity keystore: {err:#}"))?;
    Ok(Identity::new(key, chain_id))
}

/// What [`spawn_mesh_service`] hands back to the boot orchestrator: the mesh ran (always
/// — a misconfigured or busy mesh port is a fatal boot error) plus the sync-gate
/// receiver the boot sequence awaits **before** binding the RPC transport.
pub(crate) struct MeshStart {
    /// Whether the mesh relays were spawned (always `true`; kept for the heartbeat log).
    pub(crate) enabled: bool,
    /// Turns ready once every target link completed one bootstrap exchange, the local
    /// tip has reached the highest reported peer tip, and no ordered pull is armed —
    /// or stays false (the caller's boot timeout then proceeds with a warning).
    pub(crate) ready: tokio::sync::watch::Receiver<bool>,
}

/// Spawn the mesh service from the node launch path (next to the gated call into
/// `smartnet-engine-rpc-sync`, which is the only code that can start RPC sync).
///
/// Always runs: [`mesh_gate`] enforces `MESH_PORT`/`MESH_ADDR` (fatal if missing or the
/// port is below 1024) and the listener is bound *before* any task starts — a busy port is
/// a boot failure, never a silent mesh-off. Every peer-file target is dialed immediately
/// (preconnect) so the initial bootstrap catch-up runs even on a quiet chain, and the
/// returned [`MeshStart::ready`] receiver is how boot learns catch-up finished (the RPC
/// transports binds only after it, or after the boot-gate timeout). The RPC is optional
/// and unrelated to whether the mesh runs.
pub(crate) async fn spawn_mesh_service<N, A>(
    ctx: &AddOnsContext<'_, N>,
    eth_api: A,
) -> MeshStart
where
    N: FullNodeComponents<
        Types: NodeTypes<
            Primitives = EthPrimitives,
            Payload: PayloadTypes<ExecutionData = ExecutionData>,
            ChainSpec: smartnet_chainspec::EthChainSpec,
        >,
    >,
    A: FullEthApiServer<NetworkTypes = Ethereum, Primitives = EthPrimitives> + Send + Sync + 'static,
{
    let port = match mesh_gate(ctx.config.rpc.mesh_port) {
        Ok(port) => port,
        Err(reason) => {
            error!(target: "smartnet::mesh", %reason, "mesh misconfigured — refusing to boot");
            std::process::exit(1);
        }
    };

    let peers_file = ctx.config.rpc.mesh_peers_file.clone().unwrap_or_else(default_peers_file);

    // Bind first: the mesh is always on, so a busy port is a boot failure, never a silent
    // mesh-off. `MESH_ADDR` absent = all interfaces.
    let bind_addr = ctx.config.rpc.mesh_addr.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let listener = match TcpListener::bind((bind_addr, port)).await {
        Ok(listener) => listener,
        Err(err) => {
            error!(
                target: "smartnet::mesh",
                %err, port,
                "mesh listen port unavailable — the mesh is always on, refusing to boot"
            );
            std::process::exit(1);
        }
    };

    let tasks = ctx.node.task_executor().clone();

    // Operator mesh limits + timings. `MESH_HEARTBEAT` (ms) sets the keepalive ping
    // cadence; the shared silence bound (both ends) derives as heartbeat + SILENCE_MARGIN
    // so a live connection always survives at least one full ping period before either
    // side declares the peer dead. Unset → protocol defaults (500 ms / 800 ms).
    let mut limits = Limits::with_max_msg_mb(ctx.config.rpc.mesh_max_msg_mb);
    if let Some(heartbeat_ms) = ctx.config.rpc.mesh_heartbeat_ms {
        limits.keepalive = Duration::from_millis(heartbeat_ms);
        limits.silence = limits.keepalive + smartnet_mesh::protocol::SILENCE_MARGIN;
    }

    // Mesh identity: the validator's keystore, decrypted once — fatal if missing/broken
    // (peers could not authenticate us; the mesh must never boot without an identity).
    let chain_id = smartnet_chainspec::EthChainSpec::chain_id(ctx.config.chain.as_ref());
    let identity = match load_mesh_identity(ctx.config.config.as_deref(), chain_id) {
        Ok(identity) => Arc::new(identity),
        Err(reason) => {
            error!(target: "smartnet::mesh", %reason, "mesh identity unavailable — refusing to boot");
            std::process::exit(1);
        }
    };

    // In-process inject: EthApi (txs + chain reads) + consensus engine (blocks) — no
    // loopback round trip. Shared by the inbound server and every link session.
    let injector: Arc<dyn Injector> =
        Arc::new(NodeInjector::new(eth_api, ctx.beacon_engine_handle.clone()));
    let seen = Arc::new(SeenCache::default());
    let objects = Arc::new(ObjectCache::default());

    // One persistent link per peer: keepalive + backoff + the identity handshake live
    // there. The gate starts unfinalized (never ready) until the target set is seeded
    // from the peer-file snapshot below — notes can arrive any time after dialing.
    let mut spread_config = SpreadConfig::new(peers_file.clone(), port, identity.clone());
    spread_config.limits = limits;
    let gate = SyncGate::new();
    let spreader = Arc::new(Spreader::new(
        spread_config,
        injector.clone(),
        seen.clone(),
        objects.clone(),
        gate.clone(),
    ));

    let server = Arc::new(MeshServer::new(
        peers_file.clone(),
        seen.clone(),
        objects.clone(),
        injector,
        identity,
        gate.clone(),
    ));

    // One peer-file read seeds the gate's expected link ids and dials them immediately:
    // links used to spawn only on first spread, so on a quiet chain a fresh node never
    // connected out and never ran its bootstrap catch-up. Boot needs every target now.
    let targets = spreader.targets();
    gate.set_expected(
        targets.iter().map(|peer| format!("{}:{}", peer.ip, peer.port_range)),
    );
    spreader.preconnect(&targets);

    // 1. Inbound: binary WebSocket frames → protocol dispatch → seen-check → local inject.
    {
        let peers_file = peers_file.clone();
        tasks.spawn_critical_task("mesh-ws-server", async move {
            ws::log_listening(port, &peers_file, "in-process (EthApi + engine)");
            let server_config = ServerConfig { limits, ..ServerConfig::default() };
            if let Err(err) = serve(listener, server, server_config).await {
                error!(target: "smartnet::mesh", %err, "mesh WebSocket server exited");
            }
        });
    }

    // 2. Outbound blocks: canonical imports → hash + RLP → mark/store → spread (in order).
    spawn_block_relay::<N>(
        ctx.node.provider().clone(),
        spreader.clone(),
        seen.clone(),
        objects.clone(),
        &tasks,
    );

    // 3. Outbound txs: pool events → EIP-2718 bytes → mark/store → spread (detached).
    spawn_tx_relay::<N>(ctx.node.clone(), spreader, seen, objects, tasks);

    MeshStart { enabled: true, ready: gate.subscribe() }
}

/// Canon-state → mesh block rumors (see module docs). Awaited per block for ordering.
fn spawn_block_relay<N>(
    provider: N::Provider,
    spreader: Arc<Spreader>,
    seen: Arc<SeenCache>,
    objects: Arc<ObjectCache>,
    tasks: &TaskExecutor,
) where
    N: FullNodeComponents<Types: NodeTypes<Primitives = EthPrimitives>>,
{
    tasks.spawn_critical_task("mesh-block-relay", async move {
        let mut notifications = provider.canonical_state_stream();
        while let Some(notification) = notifications.next().await {
            for block in notification.committed().blocks_iter() {
                let sealed = block.sealed_block();
                let number = sealed.header().number;
                if number == 0 {
                    continue; // genesis is configuration, not gossip
                }
                // Seal-path encoding: full block RLP, exactly what `smartnet_newPayload`
                // decodes — never eth-wire frames. The mesh identity is
                // `keccak256(rlp)` alongside the announced block number.
                let mut rlp = Vec::new();
                alloy_rlp::Encodable::encode(sealed, &mut rlp);
                let rumor = Rumor::block(number, rlp);
                if !seen.check_and_mark(rumor.hash) {
                    continue; // already spread (e.g. we injected this block from a peer)
                }
                objects.insert(rumor.clone());
                let report = spreader.spread(&rumor).await;
                debug!(
                    target: "smartnet::mesh",
                    number, ?report,
                    "block rumor spread"
                );
            }
        }
        error!(target: "smartnet::mesh", "mesh block relay stream ended");
    });
}

/// Pool events → mesh tx rumors (see module docs). Detached per rumor.
fn spawn_tx_relay<N>(
    node: N,
    spreader: Arc<Spreader>,
    seen: Arc<SeenCache>,
    objects: Arc<ObjectCache>,
    tasks: TaskExecutor,
) where
    N: FullNodeComponents<Types: NodeTypes<Primitives = EthPrimitives>>,
{
    let inner = tasks.clone();
    tasks.spawn_critical_task("mesh-tx-relay", async move {
        let mut events = node.pool().all_transactions_event_listener();
        while let Some(event) = events.next().await {
            let hash = match event {
                FullTransactionEvent::Pending(hash) | FullTransactionEvent::Queued(hash, _) => hash,
                _ => continue,
            };
            for tx in node.pool().get_all(vec![hash]) {
                let rumor = Rumor::tx(tx.to_consensus().encoded_2718());
                if !seen.check_and_mark(rumor.hash) {
                    continue; // origin mark already happened (or a peer beat us to it)
                }
                objects.insert(rumor.clone());
                let spreader = spreader.clone();
                inner.spawn_task(async move {
                    let report = spreader.spread(&rumor).await;
                    debug!(target: "smartnet::mesh", %hash, ?report, "tx rumor spread");
                });
            }
        }
        error!(target: "smartnet::mesh", "mesh tx relay stream ended");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_requires_port_at_least_1024() {
        assert!(mesh_gate(None).is_err());
        assert!(mesh_gate(Some(0)).is_err());
        assert!(mesh_gate(Some(80)).is_err());
        assert!(mesh_gate(Some(1023)).is_err());
        assert_eq!(mesh_gate(Some(1024)), Ok(1024));
        assert_eq!(mesh_gate(Some(30811)), Ok(30811));
    }
}
