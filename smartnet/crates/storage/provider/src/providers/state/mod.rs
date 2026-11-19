//! [`StateProvider`](crate::StateProvider) implementations
pub(crate) mod historical;
pub(crate) mod latest;
pub(crate) mod overlay;

use alloy_primitives::Address;
use smartnet_primitives_traits::Bytecode;
use smartnet_storage_errors::provider::{ProviderError, ProviderResult};

/// Stage 6C: with the file tier enabled, a contract's code is answered **only** from its
/// per-address file (`bytecodes/{a[0..3]}/{a[3..6]}/{a[6..9]}/{a[9..]}` — raw runtime
/// bytecode, direct execution, keyed by deployed address per the operator layout).
/// Contract bytecode has **no DB fallback** (operator rule): the `Bytecodes` table is
/// never consulted when the file tier is enabled. `Ok(None)` means the code is not in the
/// files; an I/O failure is an error, never a silent miss. The table is read only in the
/// file-tier-disabled test mode, where it is the sole bytecode store.
pub(crate) fn code_from_files(address: &Address) -> ProviderResult<Option<Bytecode>> {
    let Some(base) = smartnet_file_tier::base() else { return Ok(None) };
    match smartnet_file_tier::read_bytecode(base, address).map_err(ProviderError::other)? {
        Some(bytes) => Ok(Some(Bytecode::new_raw(bytes.into()))),
        None => Ok(None),
    }
}
