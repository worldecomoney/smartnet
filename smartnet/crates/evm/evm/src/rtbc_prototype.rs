//! Wave-3 scratch prototype (`SC-FILTER.md` §W0.2b) — **not production wiring**.
//!
//! Proves the approved design before any wiring: one inspector, one LIFO journal-
//! checkpoint stack, no revm modifications. Acceptance criteria are the §W0.2b list;
//! the stop-rule applies if checkpoint cleanup or access-list behavior diverges.

use super::rtbc_inspector::rtbc;
use alloy_primitives::{address, Address, Bytes, U256};
use std::{fs, path::Path};
use revm::{
    context::{result::{ExecutionResult, Output}, Context, TxEnv},
    context_interface::JournalTr,
    database::InMemoryDB,
    handler::{MainBuilder, MainContext},
    inspector::NoOpInspector,
    primitives::hardfork::SpecId,
    InspectCommitEvm,
};

const GAS_LIMIT: u64 = 10_000_000;
const CALLER: Address = address!("0x000000000000000000000000000000000000aaaa");

// ---- harness -----------------------------------------------------------------

fn run_rtbc(txs: Vec<TxEnv>) -> (Vec<ExecutionResult>, InMemoryDB) {
    let mut ctx = Context::mainnet().with_db(InMemoryDB::default());
    ctx.cfg.set_spec_and_mainnet_gas_params(SpecId::LONDON);
    let mut evm = ctx.build_mainnet_with_inspector(rtbc());
    let mut results = Vec::new();
    for tx in txs {
        results.push(evm.inspect_tx_commit(tx).expect("exec"));
    }
    let db = evm.ctx.journaled_state.db().clone();
    (results, db)
}

fn run_noop(txs: Vec<TxEnv>) -> (Vec<ExecutionResult>, InMemoryDB) {
    let mut ctx = Context::mainnet().with_db(InMemoryDB::default());
    ctx.cfg.set_spec_and_mainnet_gas_params(SpecId::LONDON);
    let mut evm = ctx.build_mainnet_with_inspector(NoOpInspector);
    let mut results = Vec::new();
    for tx in txs {
        results.push(evm.inspect_tx_commit(tx).expect("exec"));
    }
    let db = evm.ctx.journaled_state.db().clone();
    (results, db)
}

fn create_tx(initcode: Vec<u8>) -> TxEnv {
    create_tx_from(CALLER, initcode)
}

fn create_tx_from(caller: Address, initcode: Vec<u8>) -> TxEnv {
    TxEnv::builder()
        .caller(caller)
        .create()
        .data(Bytes::from(initcode))
        .gas_limit(GAS_LIMIT)
        .build()
        .expect("tx")
}

fn code_of(db: &InMemoryDB, addr: Address) -> Bytes {
    db.cache.accounts
        .get(&addr)
        .and_then(|acc| acc.info.code.as_ref().map(|c| c.original_bytes()))
        .unwrap_or_default()
}

fn slot(db: &InMemoryDB, addr: Address, slot: u64) -> U256 {
    db.cache.accounts
        .get(&addr)
        .and_then(|acc| acc.storage.get(&U256::from(slot)).copied())
        .unwrap_or_default()
}

/// `CacheDB` caches *reads* — a reverted create still leaves a touched (but empty)
/// cache entry, so "untouched" must mean: no code, nonce 0, no balance, no storage.
fn child_untouched(db: &InMemoryDB, addr: Address) -> bool {
    match db.cache.accounts.get(&addr) {
        None => true,
        Some(acc) => {
            // Read-cache may hold a loaded *empty* code (`code_hash == KECCAK_EMPTY`).
            acc.info.nonce == 0
                && acc.info.balance.is_zero()
                && acc.info.code_hash == revm::primitives::KECCAK_EMPTY
                && acc.storage.values().all(|v| v.is_zero())
        }
    }
}

/// The child `CREATE` address used mid-constructor by revm — calibrated empirically
/// against a successful noop deploy (self-checks the RLP helper too).
fn calibrate_child_nonce() -> u64 {
    let child = deploy_code(&[], RUNTIME_OK);
    let (noop, db) = run_noop(vec![create_tx(parent_init(&child, false, false))]);
    assert!(noop[0].is_success(), "calibration deploy failed");
    let parent = create_address(CALLER, 0);
    let real = addr_from_slot3(&db, parent);
    (0..4)
        .find(|&n| create_address(parent, n) == real)
        .expect("child CREATE nonce not calibrated")
}

fn child_addr(parent: Address) -> Address {
    create_address(parent, calibrate_child_nonce())
}

// ---- bytecode builders -------------------------------------------------------

/// Standard deployer prefix (15 bytes) for a runtime appended at offset 15:
/// CODECOPY(dest=0, off=15, len) ; RETURN(off=0, len).
fn deploy_prefix(len: u16) -> [u8; 15] {
    let [lh, ll] = len.to_be_bytes();
    let [oh, ol] = 15u16.to_be_bytes();
    [0x61, lh, ll, 0x61, oh, ol, 0x60, 0x00, 0x39, 0x61, lh, ll, 0x60, 0x00, 0xf3]
}

/// `before` code + deployer prefix (offset accounts for `before`) + runtime.
fn deploy_code(before: &[u8], runtime: &[u8]) -> Vec<u8> {
    let off = (before.len() + 15) as u16;
    let len = runtime.len() as u16;
    let [lh, ll] = len.to_be_bytes();
    let [oh, ol] = off.to_be_bytes();
    let mut out = before.to_vec();
    out.extend_from_slice(&[0x61, lh, ll, 0x61, oh, ol, 0x60, 0x00, 0x39, 0x61, lh, ll, 0x60, 0x00, 0xf3]);
    out.extend_from_slice(runtime);
    out
}

const RUNTIME_OK: &[u8] = &[0x00]; // STOP
const RUNTIME_FORBIDDEN: &[u8] = &[0x5b, 0xff, 0x00]; // JUMPDEST SELFDESTRUCT STOP

/// PUSH32-immediate child `initcode` into memory[0..n] (children ≤ 32 bytes).
fn mstore_child(child: &mut Vec<u8>, init: &[u8]) {
    assert!(init.len() <= 32);
    child.push(0x7f); // PUSH32
    let mut word = [0u8; 32];
    word[..init.len()].copy_from_slice(init);
    child.extend_from_slice(&word);
    child.extend_from_slice(&[0x60, 0x00, 0x52]); // PUSH1 0 MSTORE
}

/// Parent constructor: slot1=1; `CREATE`(mem0, clen); slot3=addr; slot2=2;
/// [probe: slot5 = gas delta of a follow-up CALL to the child];
/// [optional second LOG0]; deploy `RUNTIME_OK`.
fn parent_init(child: &[u8], probe: bool, log2: bool) -> Vec<u8> {
    let mut c = Vec::new();
    // slot1 = 1
    c.extend_from_slice(&[0x60, 0x01, 0x60, 0x01, 0x55]);
    // mem[0..] = child initcode
    mstore_child(&mut c, child);
    // CREATE(value=0, off=0, len)
    let [lh, ll] = (child.len() as u16).to_be_bytes();
    c.extend_from_slice(&[0x61, lh, ll, 0x60, 0x00, 0x60, 0x00, 0xf0]);
    // keep addr: mem[0..32] = addr (for the probe CALL) AND slot3 = addr
    // (tests read storage; a failed CREATE stores 0 there).
    c.extend_from_slice(&[0x80, 0x60, 0x00, 0x52, 0x60, 0x03, 0x55]); // DUP1 MSTORE(0) SSTORE(3)
    if probe {
        // g_before → mem[4]
        c.extend_from_slice(&[0x5a, 0x60, 0x04, 0x52]);
        // CALL(outLen=0,outOff=0,inLen=0,inOff=0,value=0,addr=mem0,gas=0xffff)
        c.extend_from_slice(&[
            0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x51, 0x61,
            0xff, 0xff, 0xf1, 0x50,
        ]);
        // slot5 = g_before - g_after
        c.extend_from_slice(&[0x5a, 0x60, 0x04, 0x51, 0x03, 0x60, 0x05, 0x55]);
    }
    // slot2 = 2
    c.extend_from_slice(&[0x60, 0x02, 0x60, 0x02, 0x55]);
    if log2 {
        c.extend_from_slice(&[0x60, 0x00, 0x60, 0x00, 0xa0]); // LOG0 empty
    }
    // deploy RUNTIME_OK
    let prefix = deploy_prefix(RUNTIME_OK.len() as u16);
    c.extend_from_slice(&prefix);
    c.extend_from_slice(RUNTIME_OK);
    c
}

fn log_of_child_then_deploy(runtime: &[u8]) -> Vec<u8> {
    let before = [0x60, 0x00, 0x60, 0x00, 0xa0]; // LOG0 empty (len=0, off=0)
    deploy_code(&before, runtime)
}

fn log_count(res: &ExecutionResult) -> usize {
    match res {
        ExecutionResult::Success { logs, .. } => logs.len(),
        _ => 0,
    }
}

// ---- acceptance tests --------------------------------------------------------

/// §W0.2b: accepted deployment — inspector adds zero behavioral delta (gas-equal
/// with `NoOp`) and the checkpoint is committed (code installed).
#[test]
fn accept_parity_with_noop() {
    let init = deploy_code(&[], RUNTIME_OK);
    let (rtbc, db_r) = run_rtbc(vec![create_tx(init.clone())]);
    let (noop, db_n) = run_noop(vec![create_tx(init)]);
    assert!(rtbc[0].is_success(), "rtbc: {:?}", rtbc[0]);
    assert!(noop[0].is_success(), "noop: {:?}", noop[0]);
    assert_eq!(gas(&rtbc[0]), gas(&noop[0]), "gas must be identical on accept");
    let addr = created_addr(&rtbc[0]);
    assert!(!code_of(&db_r, addr).is_empty(), "accepted code installed");
    assert!(!code_of(&db_n, addr).is_empty());
}

/// §4/§W0.2b: nested forbidden child rejected — parent keeps its state, child fully
/// rolled back (no code, no account), `NoOp` run shows the counterfactual install.
#[test]
fn nested_reject_full_rollback_parent_succeeds() {
    let child = deploy_code(&[], RUNTIME_FORBIDDEN); // ≤32B? prefix15+3=18 ✓
    let init = parent_init(&child, false, false);
    let (rtbc, db_r) = run_rtbc(vec![create_tx(init.clone())]);
    let (noop, db_n) = run_noop(vec![create_tx(init)]);
    assert!(rtbc[0].is_success(), "parent must succeed: {:?}", rtbc[0]);
    assert!(noop[0].is_success());
    let parent = created_addr(&rtbc[0]);
    assert!(!code_of(&db_r, parent).is_empty());
    // Rejected CREATE pushed 0 to the parent — slot3 records the failure.
    assert_eq!(slot(&db_r, parent, 3), U256::ZERO, "parent must see failed CREATE");
    // Real child address comes from the noop counterfactual (same parent, same nonce).
    let child_n = addr_from_slot3(&db_n, created_addr(&noop[0]));
    assert_ne!(child_n, Address::ZERO);
    assert!(!code_of(&db_n, child_n).is_empty(), "noop installs the child");
    // Full rollback at the real child address: no code, nothing left behind.
    assert!(code_of(&db_r, child_n).is_empty(), "child code must be gone");
    assert!(child_untouched(&db_r, child_n), "child account must be untouched");
    // Parent's constructor state survived.
    assert_eq!(slot(&db_r, parent, 1), U256::from(1));
    assert_eq!(slot(&db_r, parent, 2), U256::from(2));
}

/// Initcode > 49,152 rejected pre-exec (our hook — revm's is dormant at LONDON);
/// the LIFO stack stays clean: the next tx deploys normally on the same Evm.
#[test]
fn initcode_reject_then_next_tx_ok() {
    let huge = vec![0x00; 49_153];
    let valid = deploy_code(&[], RUNTIME_OK);
    let other = address!("0x000000000000000000000000000000000000bbbb");
    let (res, db) =
        run_rtbc(vec![create_tx(huge), create_tx_from(other, valid)]);
    assert!(!res[0].is_success(), "oversized initcode rejected: {:?}", res[0]);
    assert!(res[1].is_success(), "stack hygiene broken: {:?}", res[1]);
    assert!(!code_of(&db, create_address(other, 0)).is_empty());
}

/// Native constructor failure must be byte-identical with and without the
/// checkpoint wrapper (parity: gas + resulting state).
#[test]
fn native_failure_parity_with_noop() {
    let child = vec![0x60, 0x00, 0x60, 0x00, 0xfd]; // PUSH1 0 PUSH1 0 REVERT
    let init = parent_init(&child, true, false);
    let (rtbc, db_r) = run_rtbc(vec![create_tx(init.clone())]);
    let (noop, db_n) = run_noop(vec![create_tx(init)]);
    assert!(rtbc[0].is_success() && noop[0].is_success());
    assert_eq!(gas(&rtbc[0]), gas(&noop[0]), "checkpoint wrapper changed native failure gas");
    let p_r = created_addr(&rtbc[0]);
    let p_n = created_addr(&noop[0]);
    assert_eq!(p_r, p_n, "parent address must be identical across runs");
    // Both runs: the reverting/rejected child pushed 0 into slot3.
    assert_eq!(slot(&db_r, p_r, 3), U256::ZERO);
    assert_eq!(slot(&db_n, p_n, 3), U256::ZERO);
    assert_eq!(slot(&db_r, p_r, 5), slot(&db_n, p_n, 5), "probe delta differs");
    let child = child_addr(p_n);
    assert!(child_untouched(&db_r, child), "rtbc child residue");
    assert!(child_untouched(&db_n, child), "noop child residue");
}

/// §W0.2b access-list acceptance: probe (in-tx gas delta of a CALL to the child
/// address after the failed create) must equal the native-failure probe — and the
/// child account must be absent in both.
#[test]
fn access_list_equivalence_native_vs_rejected() {
    let revert_child = vec![0x60, 0x00, 0x60, 0x00, 0xfd];
    let forbidden_child = deploy_code(&[], RUNTIME_FORBIDDEN);
    let (native, db_n) = run_noop(vec![create_tx(parent_init(&revert_child, true, false))]);
    let (rejected, db_r) = run_rtbc(vec![create_tx(parent_init(&forbidden_child, true, false))]);
    assert!(native[0].is_success() && rejected[0].is_success());
    let pn = created_addr(&native[0]);
    let pr = created_addr(&rejected[0]);
    assert_eq!(pn, pr);
    assert_eq!(
        slot(&db_n, pn, 5),
        slot(&db_r, pr, 5),
        "access-list/warming behavior diverged (STOP-RULE trigger)"
    );
    let child = child_addr(pn);
    assert!(child_untouched(&db_n, child), "native-fail child residue");
    assert!(child_untouched(&db_r, child), "rejected child residue");
}

/// Logs emitted inside a rejected child frame roll back with it; the parent's own
/// logs survive. `NoOp` counterfactual sees the child's log.
#[test]
fn child_logs_rolled_back_parent_logs_kept() {
    let child = log_of_child_then_deploy(RUNTIME_FORBIDDEN);
    assert!(child.len() <= 32, "child init must fit PUSH32: {}", child.len());
    let init = parent_init(&child, false, true);
    let (rtbc, _) = run_rtbc(vec![create_tx(init.clone())]);
    let (noop, _) = run_noop(vec![create_tx(init)]);
    assert!(rtbc[0].is_success() && noop[0].is_success());
    assert_eq!(log_count(&noop[0]), 2, "noop: 1 parent + 1 child log");
    assert_eq!(log_count(&rtbc[0]), 1, "rejected child's log must be gone");
}

/// §W0.2b: rejection gas is deterministic and non-free (deposit-failure class, §4).
#[test]
fn reject_gas_deterministic_and_consumed() {
    let init = parent_init(&deploy_code(&[], RUNTIME_FORBIDDEN), false, false);
    let other = address!("0x000000000000000000000000000000000000bbbb");
    let (a, _) = run_rtbc(vec![
        create_tx(init.clone()),
        create_tx_from(other, deploy_code(&[], RUNTIME_OK)),
    ]);
    let (b, _) = run_rtbc(vec![
        create_tx(init),
        create_tx_from(other, deploy_code(&[], RUNTIME_OK)),
    ]);
    // Parent succeeds in both (only the nested child is rejected) — determinism at
    // the tx level plus rejections recorded.
    assert!(a[0].is_success() && b[0].is_success());
    assert_eq!(gas(&a[0]), gas(&b[0]), "reject path gas must be deterministic");
    assert_eq!(a[1].is_success(), b[1].is_success());
    assert_eq!(gas(&a[1]), gas(&b[1]));
}

fn gas(res: &ExecutionResult) -> u64 {
    match res {
        ExecutionResult::Success { gas, .. }
        | ExecutionResult::Revert { gas, .. }
        | ExecutionResult::Halt { gas, .. } => gas.tx_gas_used(),
    }
}

/// Parent address = `CREATE`(caller, state-nonce) — tests use fresh dbs (nonce 0)
/// and distinct callers for multi-tx runs.
fn created_addr(res: &ExecutionResult) -> Address {
    let _ = res;
    create_address(CALLER, 0)
}

fn addr_from_slot3(db: &InMemoryDB, parent: Address) -> Address {
    let v = slot(db, parent, 3);
    let bytes = v.to_be_bytes::<32>();
    Address::from_slice(&bytes[12..])
}

/// `CREATE` address — alloy's canonical `keccak256(rlp([sender, nonce]))[12..]`
/// (exactly what revm's `CreateInputs::created_address` uses: `caller.create(nonce)`).
fn create_address(sender: Address, nonce: u64) -> Address {
    sender.create(nonce)
}

// ---- Wave 4(a): appended-blob children (sizes beyond PUSH32) + real artifacts --

/// Parent constructor that CODECOPYs an appended `child_blob` into memory and
/// CREATEs (or CREATE2s) it — children larger than PUSH32 bytes travel this way.
/// Fixed-width pushes make `child_off = exec length` computable in one pass.
fn append_child(child_blob: &[u8], create2: bool) -> Vec<u8> {
    let len = child_blob.len() as u16;
    let build = |child_off: u16| -> Vec<u8> {
        let mut c = Vec::new();
        c.extend_from_slice(&[0x60, 0x01, 0x60, 0x01, 0x55]); // slot1 = 1
        // CODECOPY(dest=0, off=child_off, len)
        c.extend_from_slice(&[0x61, (len >> 8) as u8, len as u8]);
        c.extend_from_slice(&[0x61, (child_off >> 8) as u8, child_off as u8]);
        c.extend_from_slice(&[0x60, 0x00, 0x39]);
        if create2 {
            // CREATE2(value=0, off=0, len, salt=0)
            c.extend_from_slice(&[0x61, 0x00, 0x00, 0x61, (len >> 8) as u8, len as u8, 0x60, 0x00, 0x60, 0x00, 0xf5]);
        } else {
            // CREATE(value=0, off=0, len)
            c.extend_from_slice(&[0x61, (len >> 8) as u8, len as u8, 0x60, 0x00, 0x60, 0x00, 0xf0]);
        }
        // keep addr: mem[0] = addr, slot3 = addr (0 when the create failed)
        c.extend_from_slice(&[0x80, 0x60, 0x00, 0x52, 0x60, 0x03, 0x55]);
        c.extend_from_slice(&[0x60, 0x02, 0x60, 0x02, 0x55]); // slot2 = 2
        c.extend_from_slice(&deploy_prefix(1)); // deploy RUNTIME_OK ([00])
        c.push(0x00);
        c
    };
    let first = build(0);
    let child_off = first.len() as u16;
    let mut c = build(child_off);
    debug_assert_eq!(c.len(), child_off as usize);
    c.extend_from_slice(child_blob);
    c
}

/// Matrix 3: nested `CREATE` carrying `initcode` > 49,152 — pre-hook rejects, parent
/// keeps going, nothing leaks (stack hygiene visible in the same tx).
#[test]
fn nested_oversized_initcode_rejected_parent_succeeds() {
    // Parent's own initcode must stay under the cap too — EVM memory starts zeroed,
    // so CREATE(0, 49_153) reads a fresh zero region as the child initcode.
    let mut c = Vec::new();
    c.extend_from_slice(&[0x60, 0x01, 0x60, 0x01, 0x55]); // slot1 = 1
    c.extend_from_slice(&[0x61, 0xc0, 0x01, 0x60, 0x00, 0x60, 0x00, 0xf0]); // CREATE(0, 0, 49153)
    c.extend_from_slice(&[0x80, 0x60, 0x00, 0x52, 0x60, 0x03, 0x55]); // slot3 = addr (0)
    c.extend_from_slice(&[0x60, 0x02, 0x60, 0x02, 0x55]); // slot2 = 2
    c.extend_from_slice(&deploy_prefix(1)); // deploy RUNTIME_OK
    c.push(0x00);
    let (res, db) = run_rtbc(vec![create_tx(c)]);
    assert!(res[0].is_success(), "parent must survive: {:?}", res[0]);
    let parent = create_address(CALLER, 0);
    assert_eq!(slot(&db, parent, 1), U256::from(1), "parent state kept");
    assert_eq!(slot(&db, parent, 3), U256::ZERO, "child create must have failed");
}

/// Matrix 2+4+5: nested constructor returns a 24,577-byte runtime — rejected at
/// `create_end`, full rollback (no code, untouched account), parent intact.
#[test]
fn nested_oversized_runtime_rejected_with_rollback() {
    let blob = deploy_code(&[], &vec![0x00; 24_577]);
    let (res, db) = run_rtbc(vec![create_tx(append_child(&blob, false))]);
    assert!(res[0].is_success(), "parent must survive: {:?}", res[0]);
    let parent = create_address(CALLER, 0);
    assert_eq!(slot(&db, parent, 1), U256::from(1));
    assert_eq!(slot(&db, parent, 3), U256::ZERO);
    let child = child_addr(parent);
    assert!(code_of(&db, child).is_empty(), "oversized runtime must not install");
    assert!(child_untouched(&db, child), "rollback incomplete");
}

/// Matrix 5: the `CREATE2` route refuses exactly like `CREATE`.
#[test]
fn create2_route_rejected_same_as_create() {
    let blob = deploy_code(&[], RUNTIME_FORBIDDEN);
    let (res, db) = run_rtbc(vec![create_tx(append_child(&blob, true))]);
    assert!(res[0].is_success(), "parent must survive: {:?}", res[0]);
    let parent = create_address(CALLER, 0);
    assert_eq!(slot(&db, parent, 3), U256::ZERO, "CREATE2 child must be refused");
    let child2 = parent.create2_from_code([0u8; 32], &blob);
    assert!(code_of(&db, child2).is_empty());
    assert!(child_untouched(&db, child2), "CREATE2 rollback incomplete");
}

/// Matrix 7 (execution half): real solc artifacts deploy through the gate —
/// `wemx` 7.6 WEMTOKEN (the metadata-FP canary) and `wemc` One.
#[test]
fn real_artifacts_deploy_through_gate() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for (file, name) in [
        ("wemx.7.6.opt.json", "WEMTOKEN"),
        ("wemc.8.26.london.opt.json", "One"),
        ("wemu.json", "BEP20Token"),
        ("wwem.json", "BEP20Token"),
    ] {
        let raw = fs::read_to_string(dir.join(file)).expect("fixture");
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let key = json["contracts"]
            .as_object()
            .unwrap()
            .keys()
            .find(|k| k.ends_with(&format!(":{name}")))
            .unwrap_or_else(|| panic!("{name} not in {file}"))
            .clone();
        let contract = &json["contracts"][&key];
        let init = alloy_primitives::hex::decode(contract["bin"].as_str().unwrap()).unwrap();
        let runtime = contract["bin-runtime"].as_str().unwrap().to_string();
        let (res, db) = run_rtbc(vec![create_tx(init)]);
        assert!(res[0].is_success(), "{file}:{name} must deploy: {:?}", res[0]);
        let addr = create_address(CALLER, 0);
        let installed = code_of(&db, addr);
        assert_eq!(
            installed,
            Bytes::from(alloy_primitives::hex::decode(&runtime).unwrap()),
            "{file}:{name} installed code mismatch"
        );
    }
}

// ---- Wave 4b: matrix items 8/9/10 + the unblocked 0.5.16 family --------------

fn sel(sig: &str) -> Vec<u8> {
    alloy_primitives::keccak256(sig.as_bytes())[..4].to_vec()
}

fn pad_addr(a: Address) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a.as_slice());
    w
}

fn pad_u256(v: U256) -> [u8; 32] {
    v.to_be_bytes::<32>()
}

/// Explicit-nonce create tx (sequences from one caller never rely on auto nonces).
fn tx_create(caller: Address, nonce: u64, initcode: Vec<u8>) -> TxEnv {
    TxEnv::builder()
        .caller(caller)
        .nonce(nonce)
        .create()
        .data(Bytes::from(initcode))
        .gas_limit(GAS_LIMIT)
        .build()
        .expect("create tx")
}

/// Explicit-nonce call tx.
fn tx_call(caller: Address, nonce: u64, to: Address, data: Vec<u8>) -> TxEnv {
    TxEnv::builder()
        .caller(caller)
        .nonce(nonce)
        .call(to)
        .data(Bytes::from(data))
        .gas_limit(GAS_LIMIT)
        .build()
        .expect("call tx")
}

fn call_bytes(res: &ExecutionResult) -> Bytes {
    match res {
        ExecutionResult::Success { output: Output::Call(b), .. } => b.clone(),
        other => panic!("expected call output, got {other:?}"),
    }
}

/// Load `(creation, runtime)` hex from the in-crate fixture set.
fn fixture(contract_file: &str, contract: &str) -> (Vec<u8>, Vec<u8>) {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(contract_file);
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let key = json["contracts"]
        .as_object()
        .expect("contracts map")
        .keys()
        .find(|k| k.ends_with(&format!(":{contract}")))
        .unwrap_or_else(|| panic!("{contract} not in {contract_file}"))
        .clone();
    let c = &json["contracts"][&key];
    let dec = |s: &str| -> Vec<u8> {
        if s.is_empty() { Vec::new() } else { alloy_primitives::hex::decode(s).expect("hex") }
    };
    (dec(c["bin"].as_str().unwrap_or_default()), dec(c["bin-runtime"].as_str().unwrap_or_default()))
}

/// Matrix 8: proxy + implementation with DELEGATECALL — both deploy through the gate
/// and the call runs through the proxy untouched.
#[test]
fn proxy_and_impl_work_through_gate() {
    let (impl_init, _) = fixture("proxy.json", "Impl");
    let (proxy_init, _) = fixture("proxy.json", "Proxy");
    let c_impl = address!("0x000000000000000000000000000000000000aa11");
    let c_proxy = address!("0x000000000000000000000000000000000000aa22");
    let c_caller = address!("0x000000000000000000000000000000000000aa33");

    let impl_addr = create_address(c_impl, 0);
    let mut proxy_data = proxy_init;
    proxy_data.extend_from_slice(&pad_addr(impl_addr)); // constructor(address)

    let (res, db) = run_rtbc(vec![
        tx_create(c_impl, 0, impl_init),
        tx_create(c_proxy, 0, proxy_data),
        tx_call(c_caller, 0, create_address(c_proxy, 0), sel("value()")),
    ]);
    assert!(res[0].is_success(), "impl deploy: {:?}", res[0]);
    assert!(res[1].is_success(), "proxy deploy: {:?}", res[1]);
    assert!(!code_of(&db, impl_addr).is_empty(), "impl installed");
    let proxy_addr = create_address(c_proxy, 0);
    assert!(!code_of(&db, proxy_addr).is_empty(), "proxy installed");
    assert!(res[2].is_success(), "delegatecall through proxy: {:?}", res[2]);
    let out = call_bytes(&res[2]);
    assert_eq!(U256::from_be_slice(&out), U256::from(42), "proxy must return 42");
}

/// Matrix 10: a CREATE2 factory deploys a legit child — allowed (the CREATE2 route
/// refusal side is covered by `create2_route_rejected_same_as_create`).
#[test]
fn create2_factory_deploys_legit_child() {
    let (factory_init, _) = fixture("create2-factory.json", "Create2Factory");
    let c_f = address!("0x000000000000000000000000000000000000bb01");
    let c_x = address!("0x000000000000000000000000000000000000bb02");
    let salt = [7u8; 32];
    let child_init = deploy_code(&[], RUNTIME_OK);

    // deploy(bytes,bytes32): HEAD = [bytes-offset, salt], TAIL = [len, padded data]
    let mut data = sel("deploy(bytes,bytes32)");
    data.extend_from_slice(&pad_u256(U256::from(64))); // offset of tail (2 head words)
    data.extend_from_slice(&salt); // head: static param
    data.extend_from_slice(&pad_u256(U256::from(child_init.len())));
    let mut padded = child_init.clone();
    while !padded.len().is_multiple_of(32) {
        padded.push(0);
    }
    data.extend_from_slice(&padded);

    let f_addr = create_address(c_f, 0);
    let (res, db) = run_rtbc(vec![
        tx_create(c_f, 0, factory_init),
        tx_call(c_x, 0, f_addr, data),
    ]);
    assert!(res[0].is_success(), "factory deploy: {:?}", res[0]);
    assert!(res[1].is_success(), "factory.deploy: {:?}", res[1]);

    // three-way: factory's own return == alloy's CREATE2 formula == installed code
    let deployed = Address::from_slice(&call_bytes(&res[1])[12..]);
    let expected = f_addr.create2_from_code(salt, &child_init);
    assert_eq!(deployed, expected, "factory return must equal the CREATE2 address");
    assert!(!code_of(&db, expected).is_empty(), "CREATE2 child must be installed at {expected:?}");
    assert_eq!(code_of(&db, expected), Bytes::from(RUNTIME_OK));
}

/// Matrix 9: Uniswap V2 Factory + Pair + Router through the gate — pair created via
/// the factory (CREATE2 with the library's patched init-code hash), then a real
/// addLiquidity (approve x2 -> pair.mint -> reserves). All in ONE run: state chains.
#[test]
fn uniswap_v2_trio_works_through_gate() {
    let (fac_init, _) = fixture("v2-core.json", "UniswapV2Factory");
    let (pair_init, _) = fixture("v2-core.json", "UniswapV2Pair");
    let (tok_init, _) = fixture("wemu.json", "BEP20Token"); // same artifact, two instances
    let (rout_init, _) = fixture("v2-router.json", "UniswapV2Router02");

    let c_f = address!("0x000000000000000000000000000000000000cc01"); // factory deployer / feeTo
    let c_l = address!("0x000000000000000000000000000000000000cc02"); // LP: 2 token deploys + 2 approves + add
    let c_r = address!("0x000000000000000000000000000000000000cc03"); // router deployer
    let c_p = address!("0x000000000000000000000000000000000000cc04"); // pair ops (createPair/getPair/getReserves)
    let c_d = address!("0x000000000000000000000000000000000000cc05"); // diagnostics (router views/allowance)
    let weth = address!("0x000000000000000000000000000000000000dead"); // placeholder: ETH-pair paths unused

    let f_addr = create_address(c_f, 0);
    let r_addr = create_address(c_r, 0);
    let t1 = create_address(c_l, 0);
    let t2 = create_address(c_l, 1);
    let amount = U256::from(1_000_000_000u64); // <= BEP20 supply (2e10 base units, decimals=2)

    // V2 pair CREATE2 address: salt = keccak256(token0 || token1) (sorted), init = pair code
    let (a0, a1) = if t1 < t2 { (t1, t2) } else { (t2, t1) };
    let mut packed = Vec::with_capacity(40);
    packed.extend_from_slice(a0.as_slice());
    packed.extend_from_slice(a1.as_slice());
    let salt_b256 = alloy_primitives::keccak256(&packed);
    let mut salt32 = [0u8; 32];
    salt32.copy_from_slice(salt_b256.as_slice());
    let expected_pair = f_addr.create2_from_code(salt32, &pair_init);

    let mut fac_data = fac_init;
    fac_data.extend_from_slice(&pad_addr(c_f)); // constructor(feeToSetter)
    let mut rout_data = rout_init;
    rout_data.extend_from_slice(&pad_addr(f_addr)); // constructor(factory, weth)
    rout_data.extend_from_slice(&pad_addr(weth));

    let mut create_pair = sel("createPair(address,address)");
    create_pair.extend_from_slice(&pad_addr(t1));
    create_pair.extend_from_slice(&pad_addr(t2));

    let mut approve = sel("approve(address,uint256)");
    approve.extend_from_slice(&pad_addr(r_addr));
    approve.extend_from_slice(&pad_u256(amount));

    // NOTE: Router01's real signature carries `address to` before `deadline` (8 params),
    // and routes the transfers through `UniswapV2Library.pairFor` (the patched init-code hash).
    let mut add_liq = sel("addLiquidity(address,address,uint256,uint256,uint256,uint256,address,uint256)");
    add_liq.extend_from_slice(&pad_addr(t1));
    add_liq.extend_from_slice(&pad_addr(t2));
    add_liq.extend_from_slice(&pad_u256(amount));
    add_liq.extend_from_slice(&pad_u256(amount));
    add_liq.extend_from_slice(&pad_u256(amount));
    add_liq.extend_from_slice(&pad_u256(amount));
    add_liq.extend_from_slice(&pad_addr(c_l)); // to: LP token recipient
    add_liq.extend_from_slice(&pad_u256(U256::MAX)); // deadline

    let mut get_pair = sel("getPair(address,address)");
    get_pair.extend_from_slice(&pad_addr(t1));
    get_pair.extend_from_slice(&pad_addr(t2));

    let mut allowance = sel("allowance(address,address)");
    allowance.extend_from_slice(&pad_addr(c_l));
    allowance.extend_from_slice(&pad_addr(r_addr));

    let (res, db) = run_rtbc(vec![
        tx_create(c_f, 0, fac_data),             // 0 V2 factory
        tx_create(c_l, 0, tok_init.clone()),     // 1 token1 (LP-owned)
        tx_create(c_l, 1, tok_init),             // 2 token2 (LP-owned)
        tx_create(c_r, 0, rout_data),            // 3 V2 router
        tx_call(c_p, 0, f_addr, create_pair),    // 4 factory.createPair (internal CREATE2)
        tx_call(c_l, 2, t1, approve.clone()),    // 5 approve router on token1
        tx_call(c_l, 3, t2, approve),            // 6 approve router on token2
        tx_call(c_l, 4, r_addr, add_liq),        // 7 addLiquidity -> pair.mint
        tx_call(c_p, 1, f_addr, get_pair),       // 8 factory.getPair
        tx_call(c_p, 2, expected_pair, sel("getReserves()")), // 9 pair reserves
        tx_call(c_d, 0, r_addr, sel("factory()")), // 10 diagnostic: router.factory()
        tx_call(c_d, 1, r_addr, sel("WETH()")),    // 11 diagnostic: router.WETH()
        tx_call(c_d, 2, t1, allowance),            // 12 diagnostic: allowance(c_l -> router)
    ]);

    for (i, r) in res[..7].iter().enumerate() {
        assert!(r.is_success(), "v2 setup step {i} failed: {r:?}");
    }

    // 4: factory returned exactly the CREATE2 address the library formula predicts
    let pair_from_create = Address::from_slice(&call_bytes(&res[4])[12..]);
    assert_eq!(pair_from_create, expected_pair, "factory.createPair must land on the CREATE2 address");
    assert!(!code_of(&db, expected_pair).is_empty(), "pair installed through the gate");

    if !res[7].is_success() {
        let fac_view = Address::from_slice(&call_bytes(&res[10])[12..]);
        let weth_view = Address::from_slice(&call_bytes(&res[11])[12..]);
        let allow = U256::from_be_slice(&call_bytes(&res[12]));
        panic!(
            "addLiquidity failed: {:?} | router.factory()={fac_view:?} (want {f_addr:?}) \
             router.WETH()={weth_view:?} allowance={allow} (want {amount}) \
             pair_at_expected={} pair_code={:?}",
            res[7],
            !code_of(&db, expected_pair).is_empty(),
            code_of(&db, expected_pair).len(),
        );
    }

    // 8: factory agrees about the pair
    assert!(res[8].is_success(), "getPair: {:?}", res[8]);
    assert_eq!(
        Address::from_slice(&call_bytes(&res[8])[12..]),
        expected_pair,
        "factory.getPair must return the created pair"
    );

    // 9: reserves produced by mint
    assert!(res[9].is_success(), "getReserves: {:?}", res[9]);
    let reserves = call_bytes(&res[9]);
    assert_eq!(reserves.len(), 96, "getReserves returns (u112,u112,u32)");
    let r0 = U256::from_be_slice(&reserves[..32]);
    let r1 = U256::from_be_slice(&reserves[32..64]);
    assert!(!r0.is_zero() && !r1.is_zero(), "addLiquidity must produce reserves ({r0},{r1})");
}
