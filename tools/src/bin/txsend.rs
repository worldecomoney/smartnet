//! txsend — Rust replacement for the go-tools (chainkit + evmtest), TEST keys only.
//!
//! Subcommands: status | native | erc20 | erc20-balance | deploy | balance
//! Chainkit parity: --rpc --key --chain --gas-price (auto|wei) --gas-limit (0=estimate
//! once, reuse) --sleep-ms --nonce (0=pending); burst errors resync the pending nonce
//! and continue (rejected txs never enter the pool). evmtest parity: TXS / SLEEP_MS
//! env fallbacks, final "chain head now at block N". Single strict ordered loop,
//! pool-full backpressure — never skip a nonce (CONTRACT-ASYNC-ROUTE.md).

use alloy_primitives::{Address, Bytes, TxKind, B256, U256};
use std::str::FromStr;
use std::time::{Duration, Instant};

use smartnet_tools::args::Args;
use smartnet_tools::keys::{TEST_FUNDER_PRIV, TEST_RECIPIENT};
use smartnet_tools::rpc::{classify_rpc_error, Http, RpcErrClass};
use smartnet_tools::stats::{print_report, Collector};
use smartnet_tools::tx::{self, SignedTx};
use smartnet_tools::{hex0x, CHAIN_ID, DEFAULT_RPC, GAS_PRICE_WEI};

fn usage() -> ! {
    eprintln!(
        "usage: txsend <cmd> [options]\n\
         cmds:\n\
         \x20 status            head/basefee/gasprice/balance/pool (chainkit status)\n\
         \x20 native            --to 0x.. --value WEI --count N --sleep-ms N --gas-limit N\n\
         \x20 erc20             --contract 0x.. --to 0x.. --amount N --count N --sleep-ms N\n\
         \x20 erc20-balance     --contract 0x.. [--addr 0x..]\n\
         \x20 deploy            <bytecode.hex> [--no-wait]\n\
         \x20 balance           [--addr 0x..]\n\
         common: --rpc URL (default {DEFAULT_RPC} or $TXSEND_RPC) --key HEX\n\
         \x20      --chain N (default {CHAIN_ID}) --gas-price auto|WEI (default {GAS_PRICE_WEI})\n\
         \x20      --gas-limit N (0 = estimate once, reuse) --nonce N (0 = pending)\n\
         \x20      --secure (balance pre-check) env: TXS, SLEEP_MS fallbacks"
    );
    std::process::exit(2);
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}

/// Run the single ordered loop. Pool-full → wait/retry same nonce. Any terminal
/// rejection → resync pending nonce and CONTINUE (chainkit parity: rejected txs
/// never enter the pool, so the next build reuses this nonce — no gap).
/// Per tx: sign / encode / rpc / total timings. Returns collector + last accepted hash.
fn run_loop(
    label: &str,
    http: &mut Http,
    sender_hex: &str,
    start_nonce: u64,
    build: impl Fn(u64) -> Result<SignedTx, String>,
    count: u64,
    sleep_ms: u64,
) -> (Collector, Option<B256>) {
    let mut coll = Collector::default();
    let mut nonce = start_nonce;
    let mut last_ok: Option<B256> = None;
    let mut waits: u64 = 0;
    let wall = Instant::now();
    for i in 0..count {
        let mut sample: Option<(u128, u128, u128, u128)> = None;
        let mut outcome: Option<Result<B256, String>> = None;
        let mut attempts = 0u32;
        while outcome.is_none() {
            attempts += 1;
            let t0 = Instant::now();
            let signed = match build(nonce) {
                Ok(s) => s,
                Err(e) => {
                    outcome = Some(Err(format!("build: {e}")));
                    break;
                }
            };
            let t_rpc = Instant::now();
            let res = http.send_raw(&signed.raw_hex);
            let rpc_us = t_rpc.elapsed().as_micros();
            let total_us = t0.elapsed().as_micros();
            if sample.is_none() {
                sample = Some((signed.sign_us, signed.encode_us, rpc_us, total_us));
            }
            match res {
                Ok(h) => outcome = Some(Ok(h)),
                Err(e) => match classify_rpc_error(&e) {
                    RpcErrClass::Full => {
                        waits += 1;
                        if attempts >= 2400 {
                            outcome = Some(Err(format!("pool stayed full: {e}")));
                        } else {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    }
                    RpcErrClass::Known => outcome = Some(Ok(signed.hash)),
                    RpcErrClass::TooLow => match http.nonce(sender_hex) {
                        Ok(n) => {
                            nonce = n;
                            std::thread::sleep(Duration::from_millis(50));
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
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    }
                },
            }
        }
        if let Some((sign_us, encode_us, rpc_us, total_us)) = sample {
            coll.push_steps(sign_us, encode_us, rpc_us, total_us);
        }
        match outcome {
            Some(Ok(h)) => {
                coll.tally.record_ok();
                last_ok = Some(h);
                if let Some((sign_us, encode_us, rpc_us, _)) = sample {
                    println!(
                        "[{i}] {h} nonce={nonce} sign={sign_us}µs encode={encode_us}µs rpc={rpc_us}µs"
                    );
                }
                nonce += 1;
            }
            Some(Err(e)) => {
                // chainkit parity: record, resync pending (rejected tx never pooled),
                // continue the loop with the same nonce position.
                coll.tally.record_err(&e);
                eprintln!("[{i}] err nonce={nonce}: {e} — resync, continue");
                if let Ok(n) = http.nonce(sender_hex) {
                    nonce = n;
                }
            }
            None => unreachable!(),
        }
        if sleep_ms > 0 && i + 1 < count {
            std::thread::sleep(Duration::from_millis(sleep_ms));
        }
    }
    let wall_s = wall.elapsed().as_secs_f64();
    if waits > 0 {
        println!("backpressure waits: {waits} (pool full → waited, retried same nonce)");
    }
    print_report(label, wall_s, &coll);
    (coll, last_ok)
}

fn print_head(http: &mut Http) {
    if let Ok(b) = http.block_number() {
        println!("chain head now at block {b}");
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);
    let cmd = args.cmd.clone().unwrap_or_else(|| usage());

    let rpc = args
        .get("rpc")
        .map(str::to_string)
        .or_else(|| std::env::var("TXSEND_RPC").ok())
        .unwrap_or_else(|| DEFAULT_RPC.to_string());
    let key = args.get("key").unwrap_or(TEST_FUNDER_PRIV).to_string();
    let chain: u64 = args.num("chain", CHAIN_ID);
    let sender = tx::key_address(&key).unwrap_or_else(|e| die(&e));
    let sender_hex = hex0x(sender.as_slice());

    let addr_of = |name: &str, default: &str| -> Address {
        match args.get(name) {
            Some(v) => Address::from_str(v).unwrap_or_else(|e| die(&format!("{name}: {e}"))),
            None => Address::from_str(default).unwrap_or_else(|e| die(&format!("{name}: {e}"))),
        }
    };
    let u256_of = |name: &str, default: &str| -> U256 {
        let v = args.get(name).unwrap_or(default);
        U256::from_str(v).unwrap_or_else(|e| die(&format!("{name}: {e}")))
    };
    // evmtest env parity: TXS / SLEEP_MS when the flag is absent
    let env_num = |name: &str| -> Option<u64> {
        std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok())
    };
    let count_or = |flag: &str, env: &str, default: u64| -> u64 {
        match args.num::<u64>(flag, u64::MAX) {
            u64::MAX => env_num(env).unwrap_or(default),
            n => n,
        }
    };
    let sleep_or = |flag: &str, env: &str| -> u64 {
        match args.num::<u64>(flag, u64::MAX) {
            u64::MAX => env_num(env).unwrap_or(0),
            n => n,
        }
    };

    match cmd.as_str() {
        "status" => {
            let mut http = Http::connect(&rpc).unwrap_or_else(|e| die(&e));
            let head = http.block_number().unwrap_or_else(|e| die(&e));
            let basefee = http.base_fee().unwrap_or(0);
            let gp = args
                .get("gas-price")
                .filter(|v| *v != "auto")
                .and_then(|v| v.parse::<u128>().ok())
                .map(Ok)
                .unwrap_or_else(|| http.gas_price())
                .unwrap_or(GAS_PRICE_WEI);
            let bal = http.balance(&sender_hex).unwrap_or_else(|e| die(&e));
            let coin: f64 = bal.to_string().parse::<f64>().unwrap_or(0.0) / 1e18;
            let pool = http.txpool_status();
            println!(
                "head={head} basefee={basefee} gasprice={gp} sender={sender_hex} balance={coin:.6} coin ({bal} wei)"
            );
            match pool {
                Ok(v) => println!("txpool={v}"),
                Err(e) => println!("txpool={e}"),
            }
        }
        "balance" => {
            let mut http = Http::connect(&rpc).unwrap_or_else(|e| die(&e));
            let target = addr_of("addr", &sender_hex);
            let b = http
                .balance(&hex0x(target.as_slice()))
                .unwrap_or_else(|e| die(&e));
            println!("addr={target} balance_wei={b}");
        }
        "erc20-balance" => {
            let contract = addr_of("contract", "");
            if contract.is_zero() {
                die("--contract 0x.. required");
            }
            let holder = addr_of("addr", &sender_hex);
            let mut http = Http::connect(&rpc).unwrap_or_else(|e| die(&e));
            let data = tx::erc20_balance_of_calldata(holder);
            let v = http
                .eth_call(&hex0x(contract.as_slice()), &hex0x(data.as_ref()))
                .unwrap_or_else(|e| die(&e));
            let bal = tx::parse_u256_hex(&v).unwrap_or_else(|e| die(&e));
            println!("contract={contract} holder={holder} balance={bal}");
        }
        "native" => {
            let to = addr_of("to", TEST_RECIPIENT);
            let value = u256_of("value", "10000");
            let count = count_or("count", "TXS", 1);
            let sleep_ms = sleep_or("sleep-ms", "SLEEP_MS");
            let mut http = Http::connect(&rpc).unwrap_or_else(|e| die(&e));
            let gas = resolve_gas(&mut http, &args, 21_000, &sender_hex, Some(to), value, None);
            let gas_price = resolve_gas_price(&mut http, &args);
            if args.flag("secure") {
                // SecureSendCoins parity: balance >= value + gas
                let bal = http.balance(&sender_hex).unwrap_or_else(|e| die(&e));
                let need = value + U256::from(gas) * U256::from(gas_price);
                if bal < need {
                    die("insufficient native balance");
                }
            }
            let nonce = start_nonce(&mut http, &args, &sender_hex);
            println!(
                "rpc={rpc} sender={sender_hex} start_nonce={nonce} count={count} to={to} value_wei={value} gas={gas} gas_price={gas_price} chain={chain}"
            );
            run_loop(
                &format!("txsend native count={count}"),
                &mut http,
                &sender_hex,
                nonce,
                |n| {
                    tx::sign_legacy_ex(
                        &key,
                        n,
                        TxKind::Call(to),
                        value,
                        gas,
                        Bytes::new(),
                        chain,
                        gas_price,
                    )
                },
                count,
                sleep_ms,
            );
            print_head(&mut http);
        }
        "erc20" => {
            let contract = addr_of("contract", "");
            if contract.is_zero() {
                die("--contract 0x.. required");
            }
            let to = addr_of("to", TEST_RECIPIENT);
            let amount = u256_of("amount", "1");
            let count = count_or("count", "TXS", 1);
            let sleep_ms = sleep_or("sleep-ms", "SLEEP_MS");
            let data = tx::erc20_transfer_calldata(to, amount);
            let mut http = Http::connect(&rpc).unwrap_or_else(|e| die(&e));
            // chainkit parity: default --gas-limit 0 = estimate once, reuse (WEMX
            // transfer costs ~105k gas — a fixed 60000 OOGs and can never seal).
            let gas = resolve_gas(
                &mut http,
                &args,
                0,
                &sender_hex,
                Some(contract),
                U256::ZERO,
                Some(&data),
            );
            let gas_price = resolve_gas_price(&mut http, &args);
            if args.flag("secure") {
                // SecureSendTokens parity: token balance >= amount, native >= 1e15
                let tdata = tx::erc20_balance_of_calldata(sender);
                let v = http
                    .eth_call(&hex0x(contract.as_slice()), &hex0x(tdata.as_ref()))
                    .unwrap_or_else(|e| die(&e));
                let tbal = tx::parse_u256_hex(&v).unwrap_or_else(|e| die(&e));
                if tbal < amount {
                    die("insufficient token balance");
                }
                let nat = http.balance(&sender_hex).unwrap_or_else(|e| die(&e));
                if nat < U256::from(1_000_000_000_000_000u64) {
                    die("insufficient gas balance");
                }
            }
            let nonce = start_nonce(&mut http, &args, &sender_hex);
            println!(
                "rpc={rpc} sender={sender_hex} start_nonce={nonce} count={count} contract={contract} to={to} amount={amount} gas={gas} gas_price={gas_price} chain={chain}"
            );
            run_loop(
                &format!("txsend erc20 count={count}"),
                &mut http,
                &sender_hex,
                nonce,
                |n| {
                    tx::sign_legacy_ex(
                        &key,
                        n,
                        TxKind::Call(contract),
                        U256::ZERO,
                        gas,
                        data.clone(),
                        chain,
                        gas_price,
                    )
                },
                count,
                sleep_ms,
            );
            print_head(&mut http);
        }
        "deploy" => {
            let file = args.pos.first().unwrap_or_else(|| usage());
            let raw =
                std::fs::read_to_string(file).unwrap_or_else(|e| die(&format!("{file}: {e}")));
            let cleaned: String = raw.chars().filter(|c| !c.is_ascii_whitespace()).collect();
            let code = smartnet_tools::unhex(&cleaned).unwrap_or_else(|e| die(&e));
            let code_bytes = Bytes::from(code.clone());
            let mut http = Http::connect(&rpc).unwrap_or_else(|e| die(&e));
            let gas = resolve_gas(
                &mut http,
                &args,
                4_000_000,
                &sender_hex,
                None,
                U256::ZERO,
                Some(&code_bytes),
            );
            let gas_price = resolve_gas_price(&mut http, &args);
            let nonce = start_nonce(&mut http, &args, &sender_hex);
            println!(
                "rpc={rpc} sender={sender_hex} start_nonce={nonce} deploy_bytes={} gas={gas} gas_price={gas_price} chain={chain}",
                code.len()
            );
            let (coll, hash) = run_loop(
                "txsend deploy count=1",
                &mut http,
                &sender_hex,
                nonce,
                |n| {
                    tx::sign_legacy_ex(
                        &key,
                        n,
                        TxKind::Create,
                        U256::ZERO,
                        gas,
                        code_bytes.clone(),
                        chain,
                        gas_price,
                    )
                },
                1,
                0,
            );
            let hash = match (coll.tally.ok, hash) {
                (1, Some(h)) => h,
                _ => die("deploy tx was not accepted"),
            };
            if args.flag("no-wait") {
                println!("deploy tx={hash} (not waiting)");
                return;
            }
            // waitReceipt parity: poll up to 60s, print contract address / revert
            for attempt in 0..120 {
                match http.receipt(&hash).unwrap_or_else(|e| die(&e)) {
                    Some(r) => {
                        let contract = r
                            .get("contractAddress")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("?")
                            .to_string();
                        let status =
                            r.get("status").and_then(serde_json::Value::as_str).unwrap_or("?");
                        let gas_used = r
                            .get("gasUsed")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("?");
                        println!("contract={contract} status={status} gasUsed={gas_used}");
                        if status != "0x1" {
                            die(&format!("deploy tx {hash} reverted"));
                        }
                        print_head(&mut http);
                        return;
                    }
                    None => {
                        if attempt % 4 == 0 {
                            println!("waiting for receipt... ({attempt})");
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
            die(&format!("deploy tx {hash} not mined within 60s"));
        }
        _ => usage(),
    }
}

/// chainkit `--gas-price`: absent = chain-fixed protocol price; `auto` = eth_gasPrice
/// once; explicit wei value otherwise.
fn resolve_gas_price(http: &mut Http, args: &Args) -> u128 {
    match args.get("gas-price") {
        Some("auto") => http.gas_price().unwrap_or(GAS_PRICE_WEI),
        Some(v) => v.parse::<u128>().unwrap_or(GAS_PRICE_WEI),
        None => GAS_PRICE_WEI,
    }
}

/// chainkit `--gas-limit`: 0 = estimate once and reuse; otherwise the given value
/// (default per command).
fn resolve_gas(
    http: &mut Http,
    args: &Args,
    default: u64,
    sender_hex: &str,
    to: Option<Address>,
    value: U256,
    data: Option<&Bytes>,
) -> u64 {
    let g = args.num::<u64>("gas-limit", default);
    if g != 0 {
        return g;
    }
    let value_est = if value > U256::from(u128::MAX) {
        u128::MAX
    } else {
        value.to::<u128>()
    };
    let to_hex = to.map(|a| hex0x(a.as_slice()));
    http.estimate_gas(
        sender_hex,
        to_hex.as_deref(),
        value_est,
        data.map(|d| hex0x(d.as_ref())).as_deref(),
    )
    .unwrap_or_else(|e| die(&format!("estimate gas: {e}")))
}

/// chainkit `--nonce`: 0/absent = pending nonce; explicit otherwise.
fn start_nonce(http: &mut Http, args: &Args, sender_hex: &str) -> u64 {
    match args.num::<u64>("nonce", 0) {
        0 => http.nonce(sender_hex).unwrap_or_else(|e| die(&e)),
        n => n,
    }
}
