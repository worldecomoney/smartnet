//! Merged engine / `wem_*` / `smartnet_*` RPC module (unified HTTP/WS — no separate authrpc
//! listener).

use jsonrpsee::{core::RegisterMethodError, server::RpcModule, Methods};
use smartnet_rpc_api::IntoEngineApiRpcModule;

/// Holds merged engine extension methods installed on the public HTTP/WS transports.
#[derive(Debug, Clone)]
pub struct AuthRpcModule {
    pub(crate) inner: RpcModule<()>,
}

impl AuthRpcModule {
    /// Builds a module from the engine API RPC surface.
    pub fn new(engine: impl IntoEngineApiRpcModule) -> Self {
        Self { inner: engine.into_rpc_module() }
    }

    /// Mutable access to the underlying `jsonrpsee` module.
    pub const fn module_mut(&mut self) -> &mut RpcModule<()> {
        &mut self.inner
    }

    /// Merges additional JSON-RPC methods into this module.
    pub fn merge_auth_methods(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<bool, RegisterMethodError> {
        self.module_mut().merge(other.into()).map(|_| true)
    }

    /// Removes a single method by name.
    pub fn remove_auth_method(&mut self, method_name: &'static str) -> bool {
        self.module_mut().remove_method(method_name).is_some()
    }

    /// Removes multiple methods by name.
    pub fn remove_auth_methods(&mut self, methods: impl IntoIterator<Item = &'static str>) {
        for name in methods {
            self.remove_auth_method(name);
        }
    }

    /// Replaces methods with the same names from `other`.
    pub fn replace_auth_methods(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_auth_methods(other.method_names());
        self.merge_auth_methods(other)
    }
}

/// Placeholder for the removed separate Engine/auth listener (production uses unified JSON-RPC).
#[derive(Clone, Debug)]
pub struct AuthServerHandle;

impl AuthServerHandle {
    /// Placeholder when no separate authrpc listener is started (production default).
    pub const fn noop() -> Self {
        Self
    }
}
