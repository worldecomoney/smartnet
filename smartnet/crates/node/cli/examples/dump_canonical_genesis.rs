//! Prints the code-built canonical `SmartNet` genesis JSON for a chain id and signer list.
//!
//! DEV/TEST-ONLY TOOL (`test-utils` feature — the runtime rulebook is derived from the stored
//! genesis, so any genesis file works and this generator is optional):
//! `cargo run -p smartnet-cli --features test-utils --example dump_canonical_genesis -- <chain_id>
//! <signer>...`

use std::{process::ExitCode, str::FromStr};

use alloy_primitives::Address;
use serde_json::to_string_pretty;
use smartnet_cli::canonical::smartnet_chain_spec;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: dump_canonical_genesis <chain_id> <signer_address>...";
    let Some(raw_chain_id) = args.first() else {
        eprintln!("{usage}");
        return ExitCode::FAILURE;
    };
    let chain_id = match raw_chain_id.parse::<u64>() {
        Ok(chain_id) => chain_id,
        Err(_) => {
            eprintln!("{usage}");
            return ExitCode::FAILURE;
        }
    };
    if args.len() < 2 {
        eprintln!("{usage}");
        return ExitCode::FAILURE;
    }
    let mut signers = Vec::with_capacity(args.len() - 1);
    for raw in &args[1..] {
        match Address::from_str(raw) {
            Ok(signer) => signers.push(signer),
            Err(_) => {
                eprintln!("invalid signer address: {raw}");
                return ExitCode::FAILURE;
            }
        }
    }
    let spec = smartnet_chain_spec(chain_id, signers);
    println!("{}", to_string_pretty(&spec.genesis).expect("genesis serializes"));
    ExitCode::SUCCESS
}
