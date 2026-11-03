//! Debug probe: exercise the sync `BlockSource` against a live peer with full error detail.
//! Usage: `cargo run -p smartnet-net-rpc-sync --example probe_peer -- <url>`

use alloy_provider::{Provider, ProviderBuilder};

#[tokio::main]
async fn main() {
    let url = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: probe_peer <rpc-url> (no default endpoint — pass it explicitly)");
        std::process::exit(2);
    });
    let url: url::Url = url.parse().expect("valid url");
    println!("probing {url}");
    let provider = ProviderBuilder::new().connect_http(url);
    match provider.get_block_number().await {
        Ok(n) => println!("block_number OK: {n}"),
        Err(e) => {
            println!("block_number FAILED: {e}");
            let mut src: Option<&dyn std::error::Error> = std::error::Error::source(&e);
            while let Some(s) = src {
                println!("  caused by: {s}");
                src = s.source();
            }
        }
    }
}
