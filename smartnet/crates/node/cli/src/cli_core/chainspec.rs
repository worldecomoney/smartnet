use std::{fs, marker::PhantomData, path::PathBuf, sync::Arc};

use clap::builder::TypedValueParser;

#[derive(Debug, Clone)]
struct Parser<C>(PhantomData<C>);

impl<C: ChainSpecParser> TypedValueParser for Parser<C> {
    type Value = Arc<C::ChainSpec>;

    fn parse_ref(
        &self,
        _cmd: &clap::Command,
        arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        let val =
            value.to_str().ok_or_else(|| clap::Error::new(clap::error::ErrorKind::InvalidUtf8))?;
        C::parse(val).map_err(|err| {
            let arg = arg.map(|a| a.to_string()).unwrap_or_else(|| "...".to_owned());
            let msg = format!("Invalid value '{val}' for {arg}: {err}.");
            clap::Error::raw(clap::error::ErrorKind::InvalidValue, msg)
        })
    }
}

/// The parsed chain spec together with the verbatim bytes the operator fed to the CLI —
/// a file's byte content, or the inline JSON exactly as typed. The database keeps this raw
/// record at genesis init (`GENESIS_SPEC` row) so **nothing** the genesis carried is ever
/// dropped — including keys the parser itself does not read (e.g.
/// `"calibri-lite": {period, epoch, quorum}`).
#[derive(Debug)]
pub struct GenesisWithRaw<C: ChainSpecParser> {
    /// The fully built spec (the parser's own `ChainSpec` type at the init entry).
    pub spec: Arc<C::ChainSpec>,
    /// The `GenesisSpecPayload` verbatim init bytes (the `GENESIS_SPEC` row's exact content).
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone)]
struct RawKeepingParser<C>(PhantomData<C>);

impl<C: ChainSpecParser> TypedValueParser for RawKeepingParser<C> {
    type Value = Arc<GenesisWithRaw<C>>;

    fn parse_ref(
        &self,
        _cmd: &clap::Command,
        arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        let val =
            value.to_str().ok_or_else(|| clap::Error::new(clap::error::ErrorKind::InvalidUtf8))?;
        let spec = C::parse(val).map_err(|err| {
            let arg = arg.map(|a| a.to_string()).unwrap_or_else(|| "...".to_owned());
            let msg = format!("Invalid value '{val}' for {arg}: {err}.");
            clap::Error::raw(clap::error::ErrorKind::InvalidValue, msg)
        })?;
        // The DB row carries the NORMALIZED genesis (calibri* defaults replaced in
        // the DB at init — GENESIS.md); a clique file is stored byte-for-byte.
        let raw = fs::read_to_string(PathBuf::from(val))
            .unwrap_or_else(|_| val.to_string());
        let mut value: serde_json::Value = serde_json::from_str(&raw).map_err(|err| {
            let arg = arg.map(|a| a.to_string()).unwrap_or_else(|| "...".to_owned());
            clap::Error::raw(
                clap::error::ErrorKind::InvalidValue,
                format!("Invalid value '{val}' for {arg}: {err}."),
            )
        })?;
        smartnet_chainspec::normalize_calibri_genesis_raw(&mut value).map_err(|err| {
            let arg = arg.map(|a| a.to_string()).unwrap_or_else(|| "...".to_owned());
            clap::Error::raw(
                clap::error::ErrorKind::InvalidValue,
                format!("Invalid value '{val}' for {arg}: {err}."),
            )
        })?;
        let raw = serde_json::to_vec(&value).map_err(|err| {
            let arg = arg.map(|a| a.to_string()).unwrap_or_else(|| "...".to_owned());
            clap::Error::raw(
                clap::error::ErrorKind::InvalidValue,
                format!("Invalid value '{val}' for {arg}: {err}."),
            )
        })?;
        Ok(Arc::new(GenesisWithRaw { spec, raw }))
    }
}

/// Trait for parsing chain specification data supplied by the user.
pub trait ChainSpecParser: Clone + Send + Sync + 'static {
    /// The chain specification type.
    type ChainSpec: std::fmt::Debug + Send + Sync;

    /// Parses the given string (user-supplied file path or inline JSON).
    fn parse(s: &str) -> eyre::Result<Arc<Self::ChainSpec>>;

    /// Sets the chain id on an already-built spec. The sources of truth for the chain identity
    /// live in the database (`CHAIN_ID` / `VALIDATORS` / `GENESIS_SPEC`, written at genesis
    /// init and read back at every boot — no config source, no constants). This remains for
    /// spec-building callers.
    fn set_chain_id(_spec: &mut Self::ChainSpec, _chain_id: u64) {}

    /// Builds the chain rulebook from the database: the exact `Genesis` the genesis file
    /// declared at `smartnet init` (stored in the database, the single source of truth).
    /// Any well-formed genesis just works — no built-in default genesis, no chain-id or signer
    /// constants in the code, no genesis file embedded, read, or referenced at runtime.
    /// This default errors; `SmartNet` parsers override it with a database-derived rulebook.
    fn smartnet_chain_spec_from_db(
        genesis: alloy_genesis::Genesis,
    ) -> eyre::Result<Arc<Self::ChainSpec>> {
        let _ = genesis;
        eyre::bail!("chain rulebook does not derive from the stored genesis for this parser");
    }

    /// Produces a [`TypedValueParser`] for this chain spec parser.
    fn parser() -> impl TypedValueParser<Value = Arc<Self::ChainSpec>> {
        Parser(PhantomData::<Self>)
    }

    /// Produces a [`TypedValueParser`] that keeps the operator's verbatim genesis bytes
    /// (file bytes or the inline JSON exactly as typed) next to the parsed spec.
    fn parser_raw() -> impl TypedValueParser<Value = Arc<GenesisWithRaw<Self>>> {
        RawKeepingParser(PhantomData::<Self>)
    }
}

/// A helper to parse a [`Genesis`](alloy_genesis::Genesis) as argument or from disk.
///
/// Inline JSON is detected immediately (no filesystem probe); otherwise `s` is treated as a path to
/// a genesis JSON file. This makes the embedded canonical genesis resolve instantly at startup.
pub fn parse_genesis(s: &str) -> eyre::Result<alloy_genesis::Genesis> {
    let raw = if s.trim_start().starts_with('{') {
        s.to_string()
    } else {
        fs::read_to_string(PathBuf::from(s))?
    };
    // Calibri* defaults (GENESIS.md): absent keys are filled here — the same
    // normalization the DB row gets at init, so every parse path agrees.
    let mut value: serde_json::Value = serde_json::from_str(&raw)?;
    smartnet_chainspec::normalize_calibri_genesis_raw(&mut value)
        .map_err(|err| eyre::eyre!("invalid genesis: {err}"))?;
    let genesis: alloy_genesis::Genesis = serde_json::from_value(value)?;
    Ok(genesis)
}
