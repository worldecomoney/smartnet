//! RPC conversion traits for Ethereum types (merged from the former `smartnet-rpc-traits` crate).
//!
//! This crate provides traits for converting between consensus-layer types and RPC response types.


mod signable;
pub use signable::{SignTxRequestError, SignableTxRequest};

mod header;
pub use header::FromConsensusHeader;

mod transaction;
pub use transaction::{FromConsensusTx, TryIntoSimTx, TxInfoMapper};
