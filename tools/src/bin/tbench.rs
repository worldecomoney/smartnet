//! tbench — concurrent tx load generator benchmarking every meaningful step
//! (sign / encode / RPC round trip / total) with full pipeline checks intact.
//!
//! Single strict ordered loop per key (native route, CONTRACT-ASYNC-ROUTE.md):
//! one keep-alive connection per stripe, each stripe owns its own ordered nonce
//! stream. Pool-full → wait and retry the same nonce (backpressure, never a gap).
//! ERC-20 mode is a separate mode and never mixes into native timing.
//! TEST keys only (tools/README.md).

use alloy_primitives::{Address, Bytes, TxKind, U256};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Instant;

use smartnet_tools::args::Args;
use smartnet_tools::keys::{TEST_FUNDER_ADDR, TEST_KEYS, TEST_RECIPIENT};
use smartnet_tools::rpc::{classify_rpc_error, Http, RpcErrClass};
use smartnet_tools::stats::{print_report, Collector};
use smartnet_tools::tx;
use smartnet_tools::{hex0x, DEFAULT_RPC};

fn usage() -> ! {
    eprintln!(
        "usage: tbench [options]\n\
         \x20 --txs N           offered txs total            (default 10000)\n\
         \x20 --conns N         ordered loops (== stripes)   (default 1)\n\
         \x20 --stripes N       seed keys, 1..11, must == conns (default 1 = funder key)\n\
         \x20 --rpc URL         default {DEFAULT_RPC} or $TXSEND_RPC\n\
         \x20 --value WEI       native value                 (default 10000)\n\
         \x20 --to ADDR         native recipient             (default seed[10])\n\
         \x20 --erc20 CONTRACT  ERC-20 mode: token address (separate mode, async-route)\n\
         \x20 --amount N        ERC-20 transfer amount       (default 1)\n\
         \x20 --gas-limit N     21000 native / 60000 erc20\n\
         note: one ordered loop per key — use --conns N --stripes N together"
    );
    std::process::exit(2);
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);
    let cmd = args.cmd.as_deref().unwrap_or("native");
    if cmd != "native" && cmd != "erc20" && !args.flag("erc20") {
        usage();
    }

    let rpc = args
        .get("rpc")
        .map(str::to_string)
        .or_else(|| std::env::var("TXSEND_RPC").ok())
        .unwrap_or_else(|| DEFAULT_RPC.to_string());
    let txs: u64 = args.num("txs", 10_000);
    let conns: usize = args.num("conns", 1);
    let stripes_req: usize = args.num("stripes", 1);
    if conns == 0 || txs == 0 {
        die("--conns and --txs must be > 0");
    }
    if stripes_req == 0 || stripes_req > TEST_KEYS.len() {
        die(&format!("--stripes must be 1..={}", TEST_KEYS.len()));
    }
    // one strict ordered loop per key: connections and stripes must match,
    // otherwise multiple loops would share one nonce stream (scrambled = gaps).
    if conns != stripes_req {
        die(&format!(
            "single ordered loop per key: --conns must equal --stripes (got conns={conns}, stripes={stripes_req}); \
             use --conns 1 --stripes 1 for one loop, or --conns N --stripes N for N keys"
        ));
    }
    let stripes = stripes_req;
    let conns = conns;

    let is_erc20 = cmd == "erc20" || args.flag("erc20");
    let contract: Option<Address> = match args.get("contract").or_else(|| args.get("erc20")) {
        Some(v) => Some(Address::from_str(v).unwrap_or_else(|e| die(&format!("--contract: {e}")))),
        None => None,
    };
    if is_erc20 && contract.is_none() {
        die("erc20 mode requires --contract 0x..");
    }
    let to = match args.get("to") {
        Some(v) => Address::from_str(v).unwrap_or_else(|e| die(&format!("--to: {e}"))),
        None => Address::from_str(TEST_RECIPIENT).expect("seed recipient"),
    };
    let value = match args.get("value") {
        Some(v) => U256::from_str(v).unwrap_or_else(|e| die(&format!("--value: {e}"))),
        None => U256::from(10_000u64),
    };
    let amount = match args.get("amount") {
        Some(v) => U256::from_str(v).unwrap_or_else(|e| die(&format!("--amount: {e}"))),
        None => U256::from(1u64),
    };
    let gas_arg: u64 = args.num("gas-limit", 0);

    // keys + starting nonces (pending) per stripe — TEST keys only
    let keys: Vec<String> = (0..stripes).map(|i| TEST_KEYS[i].2.to_string()).collect();
    let addrs: Vec<String> = (0..stripes)
        .map(|i| {
            let a = tx::key_address(&keys[i]).unwrap_or_else(|e| die(&e));
            hex0x(a.as_slice())
        })
        .collect();
    let mut ctrl = Http::connect(&rpc).unwrap_or_else(|e| die(&e));
    let start_nonces: Vec<u64> = addrs
        .iter()
        .map(|a| ctrl.nonce(a).unwrap_or_else(|e| die(&format!("nonce {a}: {e}"))))
        .collect();
    for (i, (a, n)) in addrs.iter().zip(&start_nonces).enumerate() {
        println!("stripe[{i}] key={a} start_nonce={n}");
    }
    // 0/absent = estimate once, reuse (chainkit parity); native defaults to 21000.
    let gas: u64 = if gas_arg != 0 {
        gas_arg
    } else if is_erc20 {
        let contract = contract.expect("checked");
        let data = tx::erc20_transfer_calldata(to, amount);
        ctrl.estimate_gas(
            &addrs[0],
            Some(hex0x(contract.as_slice()).as_str()),
            0,
            Some(hex0x(data.as_ref()).as_str()),
        )
        .unwrap_or_else(|e| die(&format!("estimate gas: {e}")))
    } else {
        21_000
    };
    let mode = if is_erc20 {
        format!("erc20 contract={}", contract.expect("checked"))
    } else {
        "native".to_string()
    };
    println!(
        "mode={mode} rpc={rpc} txs={txs} loops={conns} stripes={stripes} gas={gas} (test keys only)"
    );

    let nonces: Arc<Vec<AtomicU64>> = Arc::new(
        start_nonces.iter().map(|n| AtomicU64::new(*n)).collect::<Vec<_>>(),
    );
    let offered = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(conns + 1));

    let mut handles = Vec::with_capacity(conns);
    for i in 0..conns {
        let stripe = i; // conns == stripes: thread i owns stripe i exactly
        let key = keys[stripe].clone();
        let addr = addrs[stripe].clone();
        let rpc = rpc.clone();
        let contract = contract;
        let to = to;
        let value = value;
        let amount = amount;
        let gas = gas;
        let erc20 = is_erc20;
        let nonces = Arc::clone(&nonces);
        let offered = Arc::clone(&offered);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let mut http = match Http::connect(&rpc) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("loop {i}: connect failed: {e}");
                    barrier.wait();
                    return (Collector::default(), 0u64);
                }
            };
            let mut coll = Collector::default();
            let mut waits: u64 = 0;
            barrier.wait(); // all loops connected → start clock together
            'stream: loop {
                let n = offered.fetch_add(1, Ordering::Relaxed);
                if n >= txs {
                    break;
                }
                let mut nonce = nonces[stripe].load(Ordering::Relaxed);
                let mut attempts = 0u32;
                let mut outcome: Option<Result<alloy_primitives::B256, String>> = None;
                let mut sample: Option<(u128, u128, u128, u128)> = None;
                while outcome.is_none() {
                    attempts += 1;
                    let t0 = Instant::now();
                    let build = if erc20 {
                        let data = tx::erc20_transfer_calldata(to, amount);
                        tx::sign_legacy(
                            &key,
                            nonce,
                            TxKind::Call(contract.expect("erc20 checked")),
                            U256::ZERO,
                            gas,
                            data,
                        )
                    } else {
                        tx::sign_legacy(&key, nonce, TxKind::Call(to), value, gas, Bytes::new())
                    };
                    let signed = match build {
                        Err(e) => {
                            outcome = Some(Err(format!("build: {e}")));
                            break;
                        }
                        Ok(s) => s,
                    };
                    let t_rpc = Instant::now();
                    let res = http.send_raw(&signed.raw_hex);
                    let rpc_us = t_rpc.elapsed().as_micros();
                    let total_us = t0.elapsed().as_micros();
                    if sample.is_none() {
                        sample = Some((signed.sign_us, signed.encode_us, rpc_us, total_us));
                    }
                    match res {
                        Ok(_) => outcome = Some(Ok(signed.hash)),
                        Err(e) => match classify_rpc_error(&e) {
                            RpcErrClass::Full => {
                                waits += 1;
                                if attempts >= 2400 {
                                    outcome = Some(Err(format!("pool stayed full: {e}")));
                                } else {
                                    std::thread::sleep(std::time::Duration::from_millis(50));
                                }
                            }
                            RpcErrClass::Known => outcome = Some(Ok(signed.hash)),
                            RpcErrClass::TooLow => match http.nonce(&addr) {
                                Ok(refetched) => {
                                    nonce = refetched;
                                    std::thread::sleep(std::time::Duration::from_millis(50));
                                    if attempts >= 10 {
                                        outcome = Some(Err(e));
                                    }
                                }
                                Err(e2) => outcome = Some(Err(e2)),
                            },
                            RpcErrClass::Fatal => {
                                if attempts >= 3 {
                                    outcome = Some(Err(e));
                                } else {
                                    std::thread::sleep(std::time::Duration::from_millis(50));
                                }
                            }
                        },
                    }
                }
                if let Some((s, e, r, t)) = sample {
                    coll.push_steps(s, e, r, t);
                }
                match outcome {
                    Some(Ok(_)) => {
                        coll.tally.record_ok();
                        nonces[stripe].store(nonce + 1, Ordering::Relaxed);
                    }
                    Some(Err(e)) => {
                        coll.tally.record_err(&e);
                        eprintln!(
                            "loop {i} FATAL nonce={nonce}: {e} — stopping this stream (no nonce skipped)"
                        );
                        break 'stream;
                    }
                    None => unreachable!(),
                }
            }
            (coll, waits)
        }));
    }

    barrier.wait(); // release loops together
    let wall = Instant::now();
    let mut coll = Collector::default();
    let mut total_waits: u64 = 0;
    for h in handles {
        match h.join() {
            Ok((c, w)) => {
                coll.steps.sign.extend(c.steps.sign);
                coll.steps.encode.extend(c.steps.encode);
                coll.steps.rpc.extend(c.steps.rpc);
                coll.steps.total.extend(c.steps.total);
                coll.tally.merge(&c.tally);
                total_waits += w;
            }
            Err(_) => eprintln!("worker panicked"),
        }
    }
    let wall_s = wall.elapsed().as_secs_f64();
    if total_waits > 0 {
        println!("backpressure waits: {total_waits} (pool full → waited, retried same nonce)");
    }
    print_report(
        &format!("tbench mode={mode} txs={txs} loops={conns} stripes={stripes}"),
        wall_s,
        &coll,
    );
    let _ = TEST_FUNDER_ADDR;
}
