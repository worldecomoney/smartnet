use alloy_consensus::BlobTransactionSidecar;
use alloy_eips::eip7594::BlobTransactionSidecarEip7594;

/// Converts blob sidecars to the EIP-7594 format.
///
/// Converting a sidecar requires a KZG verifier, which this build does not have, so conversion
/// always fails.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct BlobSidecarConverter;

impl BlobSidecarConverter {
    /// Creates a new blob sidecar converter.
    pub const fn new() -> Self {
        Self
    }

    /// Converts the blob sidecar to the EIP-7594 format.
    pub async fn convert(
        &self,
        _sidecar: BlobTransactionSidecar,
    ) -> Option<BlobTransactionSidecarEip7594> {
        None
    }
}
