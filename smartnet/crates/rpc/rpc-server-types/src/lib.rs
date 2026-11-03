//! `SmartNet` RPC server types.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

/// Common RPC constants.
pub mod constants;
pub mod result;

mod module;
pub use module::{
    DefaultRpcModuleValidator, LenientRpcModuleValidator, RpcModuleSelection, RpcModuleValidator,
    SmartnetRpcModule,
};

pub use result::ToRpcResult;
