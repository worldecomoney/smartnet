//! Contains RPC handler implementations specific to block access lists.

use smartnet_rpc_convert::RpcConvert;
use smartnet_rpc_eth_api::{helpers::bal::GetBlockAccessList, FromEvmError, RpcNodeCore};
use smartnet_rpc_eth_types::EthApiError;

use crate::EthApi;

impl<N, Rpc> GetBlockAccessList for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
}
