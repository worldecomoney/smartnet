#![allow(missing_docs)]

#[global_allocator]
static ALLOC: smartnet::allocator::Allocator = smartnet::allocator::new_allocator();

use clap::Parser;
use smartnet_cli::{ext::SmartnetCli, Commands};
use smartnet_node_ethereum::EthereumNode;

fn main() {
    // Install the pure-Rust TLS provider before any rustls client is built;
    // rustls panics if no process-level CryptoProvider is available.
    smartnet_tls_provider::install_default()
        .unwrap_or_else(|_| panic!("failed to install SmartNet TLS provider"));

    smartnet::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    let cli = SmartnetCli::parse();
    // `smartnet import <file>`: the import itself runs in the CLI arm BEFORE the node
    // boots (the chain file is the only writer, no engine/mesh/heartbeat exists yet), so
    // by the time the launcher runs there is nothing left to do — resolving the future
    // shuts the node down again. Without the subcommand the node runs until the exit
    // signal, as before.
    let import_file = matches!(cli.command, Some(Commands::Import(_)));
    if let Err(err) = cli.run(async move |builder, _| {
        let handle = builder.launch_node(EthereumNode::default()).await?;

        if import_file {
            // The import completed during launch; booting only ever existed to run on
            // top of the imported chain. Exit (the runner performs the clean shutdown).
            Ok(())
        } else {
            // When the shared config carries `SEAL = true`, the node's heartbeat runs the
            // validator beat loop (see `EthereumAddOns::launch_add_ons`); a malformed
            // validator section still aborts boot. Mesh links keep themselves alive.
            handle.wait_for_node_exit().await
        }
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    /// The shipped node binary must compile the Fjall storage engine — the only engine
    /// (the `sqlite`/`redb` engines were removed; `DB_ENGINE` still resolves at runtime
    /// through `DatabaseBackend`, never at build time).
    #[test]
    #[allow(clippy::assertions_on_constants)]  // build canary: asserts a compile-time flag
    fn fjall_is_compiled_into_the_node_binary() {
        assert!(smartnet_db::FJALL_COMPILED, "node binary is missing the Fjall storage engine");
    }
}
