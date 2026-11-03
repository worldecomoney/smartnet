//! Submit pre-created block RLP payloads from numbered `.dat` files only.

mod datdir;
mod decode;
mod hash;
mod head;
mod stage;
mod submit;

use clap::Parser;
use decode::inspect_block_rlp;
use eyre::{Context, Result};
use hash::payload_hash;
use stage::ValidationStage;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "smartnet-test-util",
    about = "Submit pre-created .dat block RLP payloads to wem_newPayload (no auto-generation)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Load `N.dat` from `DATA_DIR` (or one file with `--only`), submit via `wem_newPayload`.
    FuzzPayload {
        data_dir: PathBuf,
        /// RPC endpoint to submit to (no default — pass it explicitly).
        #[arg(long, value_name = "URL")]
        node_url: String,
        /// Submit only this numbered file, e.g. `16` for `16.dat`.
        #[arg(long, value_name = "N")]
        only: Option<u64>,
    },
    /// Decode a `.dat` file and print hashes (correlate with node `Received new payload` /
    /// want-got).
    InspectDat { path: PathBuf },
    /// Print canonical head and suggested `--dump-payload` filename for the next block.
    Head {
        /// RPC endpoint to submit to (no default — pass it explicitly).
        #[arg(long, value_name = "URL")]
        node_url: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::FuzzPayload { data_dir, node_url, only } => {
            run_fuzz(&node_url, &data_dir, only).await?
        }
        Command::InspectDat { path } => run_inspect(&path)?,
        Command::Head { node_url } => head::print_head(&node_url).await?,
    }
    Ok(())
}

fn run_inspect(path: &PathBuf) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let rep = inspect_block_rlp(&bytes)?;
    println!("file: {}", path.display());
    println!("rlp_bytes: {}", bytes.len());
    println!("rlp_keccak: {}", rep.rlp_keccak);
    println!("header_hash_slow: {}", rep.header_hash_slow);
    println!("state_root: {}", rep.state_root);
    println!("transactions_root: {}", rep.transactions_root);
    println!("receipts_root: {}", rep.receipts_root);
    println!("number: {}", rep.number);
    println!("extra_data_len: {}", rep.extra_data_len);
    println!();
    println!("Node log correlation:");
    println!("  'Received new payload' hash=  -> envelope block_hash (node 'got' on mismatch)");
    println!("  'want' on block hash mismatch -> seal_slow() recomputed from decoded header");
    println!("  'got'  on block hash mismatch -> envelope block_hash (should match header_hash_slow if consistent)");
    Ok(())
}

async fn run_fuzz(node_url: &str, data_dir: &Path, only: Option<u64>) -> Result<()> {
    let mut files = datdir::load_numbered_dat(data_dir)
        .with_context(|| format!("load {}", data_dir.display()))?;
    if let Some(n) = only {
        files.retain(|f| f.name == format!("{n}.dat"));
        if files.is_empty() {
            eyre::bail!("no {n}.dat in {}", data_dir.display());
        }
    }
    if files.is_empty() {
        eyre::bail!("no numbered .dat files in {} (expected 1.dat, 2.dat, …)", data_dir.display());
    }

    println!("case_name\trlp_keccak\theader_hash_slow\tstatus\tvalidationError\tvalidation_stage");

    eprintln!("submitting {} payload(s) to {node_url}", files.len());
    eprintln!(
        "(node 'Received new payload' hash should equal header_hash_slow for consistent blocks)\n"
    );

    for f in &files {
        let rep = inspect_block_rlp(&f.bytes).with_context(|| format!("inspect {}", f.name))?;
        let rlp_id = payload_hash(&f.bytes);
        let res = submit::submit_rlp_verbose(node_url, &f.name, f.bytes.clone()).await;
        let stage = ValidationStage::classify(&res.status, res.validation_error.as_deref());
        let err = res.validation_error.as_deref().unwrap_or("");
        let err_one_line = err.replace(['\n', '\t'], " ");
        println!(
            "{}\t{rlp_id}\t{}\t{}\t{err_one_line}\t{}",
            f.name,
            rep.header_hash_slow,
            res.status,
            stage.as_str()
        );
        eprintln!(
            "  {} -> header_hash_slow={} (compare to node log hash= on receive)",
            f.name, rep.header_hash_slow
        );
    }

    Ok(())
}
