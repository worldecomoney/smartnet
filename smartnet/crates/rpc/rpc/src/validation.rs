use alloy_consensus::{BlockHeader, Transaction, TxReceipt};
use alloy_eips::{
    eip6110::DEPOSIT_REQUEST_TYPE,
    eip7002::WITHDRAWAL_REQUEST_TYPE,
    eip7251::CONSOLIDATION_REQUEST_TYPE,
    eip7685::{Requests, RequestsOrHash},
};
use alloy_primitives::map::AddressSet;
use alloy_rpc_types_beacon::{
    relay::{
        BidTrace, BuilderBlockValidationRequest, BuilderBlockValidationRequestV2,
        BuilderBlockValidationRequestV3, BuilderBlockValidationRequestV4,
        BuilderBlockValidationRequestV5,
    },
    requests::ExecutionRequestsV4,
};
use alloy_rpc_types_engine::{
    BlobsBundleV1, BlobsBundleV2, CancunPayloadFields, ExecutionData, ExecutionPayload,
    ExecutionPayloadSidecar, PraguePayloadFields,
};
use async_trait::async_trait;
use core::fmt;
use jsonrpsee::core::RpcResult;
use jsonrpsee_types::error::ErrorObject;
use smartnet_evm::{execute::Executor, ConfigureEvm};
use smartnet_execution_types::BlockExecutionOutput;
use smartnet_primitives_traits::{
    constants::GAS_LIMIT_BOUND_DIVISOR, BlockBody, GotExpected, NodePrimitives, RecoveredBlock,
    SealedBlock, SealedHeaderFor,
};
use smartnet_revm::{cached::CachedReads, database::StateProviderDatabase};
use smartnet_transaction_pool::BlobTransactionValidationError;
use revm_primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use smartnet_chainspec::{ChainSpecProvider, EthereumHardforks};
use smartnet_consensus::{Consensus, FullConsensus};
use smartnet_primitives_traits::constants::MAX_RLP_BLOCK_SIZE;
use smartnet_engine_primitives::PayloadValidator;
use smartnet_errors::{BlockExecutionError, ConsensusError, ProviderError};
use smartnet_metrics::{
    metrics,
    metrics::{gauge, Gauge},
    Metrics,
};
use smartnet_node_api::{NewPayloadError, PayloadTypes};
use smartnet_rpc_api::BlockSubmissionValidationApiServer;
use smartnet_rpc_server_types::result::{internal_rpc_err, invalid_params_rpc_err};
use smartnet_storage_api::{BlockReaderIdExt, StateProviderFactory};
use smartnet_tasks::Runtime;
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};
use tracing::warn;

/// SSZ-encodes the given [`ExecutionRequestsV4`] into an EIP-7685 [`Requests`] container.
///
/// This mirrors `alloy_rpc_types_beacon::requests::ExecutionRequestsV4::to_requests`, but
/// without requiring the `ssz` feature of `alloy-rpc-types-beacon`. All Electra request
/// items are fixed-size, so their SSZ encoding is the plain concatenation of the fields in
/// declaration order (`u64`s little-endian), prefixed with the request type byte; empty
/// request types are omitted.
fn execution_requests_to_requests(execution_requests: &ExecutionRequestsV4) -> Requests {
    let mut deposits =
        Vec::with_capacity(execution_requests.deposits.len() * (48 + 32 + 8 + 96 + 8));
    for deposit in &execution_requests.deposits {
        deposits.extend_from_slice(deposit.pubkey.as_slice());
        deposits.extend_from_slice(deposit.withdrawal_credentials.as_slice());
        deposits.extend_from_slice(&deposit.amount.to_le_bytes());
        deposits.extend_from_slice(deposit.signature.as_slice());
        deposits.extend_from_slice(&deposit.index.to_le_bytes());
    }

    let mut withdrawals = Vec::with_capacity(execution_requests.withdrawals.len() * (20 + 48 + 8));
    for withdrawal in &execution_requests.withdrawals {
        withdrawals.extend_from_slice(withdrawal.source_address.as_slice());
        withdrawals.extend_from_slice(withdrawal.validator_pubkey.as_slice());
        withdrawals.extend_from_slice(&withdrawal.amount.to_le_bytes());
    }

    let mut consolidations =
        Vec::with_capacity(execution_requests.consolidations.len() * (20 + 48 + 48));
    for consolidation in &execution_requests.consolidations {
        consolidations.extend_from_slice(consolidation.source_address.as_slice());
        consolidations.extend_from_slice(consolidation.source_pubkey.as_slice());
        consolidations.extend_from_slice(consolidation.target_pubkey.as_slice());
    }

    let mut requests = Requests::with_capacity(3);
    requests.push_request_with_type(DEPOSIT_REQUEST_TYPE, deposits);
    requests.push_request_with_type(WITHDRAWAL_REQUEST_TYPE, withdrawals);
    requests.push_request_with_type(CONSOLIDATION_REQUEST_TYPE, consolidations);
    requests
}

/// The type that implements the `validation` rpc namespace trait
#[derive(Clone, Debug, derive_more::Deref)]
pub struct ValidationApi<Provider, E: ConfigureEvm, T: PayloadTypes> {
    #[deref]
    inner: Arc<ValidationApiInner<Provider, E, T>>,
}

impl<Provider, E, T> ValidationApi<Provider, E, T>
where
    E: ConfigureEvm,
    T: PayloadTypes,
{
    /// Create a new instance of the [`ValidationApi`]
    pub fn new(
        provider: Provider,
        consensus: Arc<dyn FullConsensus<E::Primitives>>,
        evm_config: E,
        config: ValidationApiConfig,
        task_spawner: Runtime,
        payload_validator: Arc<
            dyn PayloadValidator<T, Block = <E::Primitives as NodePrimitives>::Block>,
        >,
    ) -> Self {
        let ValidationApiConfig { disallow, validation_window } = config;

        let inner = Arc::new(ValidationApiInner {
            provider,
            consensus,
            payload_validator,
            evm_config,
            disallow,
            validation_window,
            cached_state: Default::default(),
            task_spawner,
            metrics: Default::default(),
        });

        inner.metrics.disallow_size.set(inner.disallow.len() as f64);

        let disallow_hash = hash_disallow_list(&inner.disallow);
        let hash_gauge = gauge!("builder_validation_disallow_hash", "hash" => disallow_hash);
        hash_gauge.set(1.0);

        Self { inner }
    }

    /// Returns the cached reads for the given head hash.
    async fn cached_reads(&self, head: B256) -> CachedReads {
        let cache = self.inner.cached_state.read().await;
        if cache.0 == head {
            cache.1.clone()
        } else {
            Default::default()
        }
    }

    /// Updates the cached state for the given head hash.
    async fn update_cached_reads(&self, head: B256, cached_state: CachedReads) {
        let mut cache = self.inner.cached_state.write().await;
        if cache.0 == head {
            cache.1.extend(cached_state);
        } else {
            *cache = (head, cached_state)
        }
    }
}

impl<Provider, E, T> ValidationApi<Provider, E, T>
where
    Provider: BlockReaderIdExt<Header = <E::Primitives as NodePrimitives>::BlockHeader>
        + ChainSpecProvider<ChainSpec: EthereumHardforks>
        + StateProviderFactory
        + 'static,
    E: ConfigureEvm + 'static,
    T: PayloadTypes<ExecutionData = ExecutionData>,
{
    /// Validates the given block and a [`BidTrace`] against it.
    pub async fn validate_message_against_block(
        &self,
        block: RecoveredBlock<<E::Primitives as NodePrimitives>::Block>,
        message: BidTrace,
        registered_gas_limit: u64,
    ) -> Result<(), ValidationApiError> {
        self.validate_message_against_header(block.sealed_header(), &message)?;

        self.consensus.validate_header(block.sealed_header())?;
        self.consensus.validate_block_pre_execution(block.sealed_block())?;

        if !self.disallow.is_empty() {
            if self.disallow.contains(&block.beneficiary()) {
                return Err(ValidationApiError::Blacklist(block.beneficiary()));
            }
            if self.disallow.contains(&message.proposer_fee_recipient) {
                return Err(ValidationApiError::Blacklist(message.proposer_fee_recipient));
            }
            for (sender, tx) in block.senders_iter().zip(block.body().transactions()) {
                if self.disallow.contains(sender) {
                    return Err(ValidationApiError::Blacklist(*sender));
                }
                if let Some(to) = tx.to() &&
                    self.disallow.contains(&to)
                {
                    return Err(ValidationApiError::Blacklist(to));
                }
            }
        }

        let latest_header =
            self.provider.latest_header()?.ok_or_else(|| ValidationApiError::MissingLatestBlock)?;

        let parent_header = if block.parent_hash() == latest_header.hash() {
            latest_header
        } else {
            // parent is not the latest header so we need to fetch it and ensure it's not too old
            let parent_header = self
                .provider
                .sealed_header_by_hash(block.parent_hash())?
                .ok_or_else(|| ValidationApiError::MissingParentBlock)?;

            if latest_header.number().saturating_sub(parent_header.number()) >
                self.validation_window
            {
                return Err(ValidationApiError::BlockTooOld);
            }
            parent_header
        };

        self.consensus.validate_header_against_parent(block.sealed_header(), &parent_header)?;
        self.validate_gas_limit(registered_gas_limit, &parent_header, block.sealed_header())?;
        let parent_header_hash = parent_header.hash();
        let state_provider = self.provider.state_by_block_hash(parent_header_hash)?;

        let mut request_cache = self.cached_reads(parent_header_hash).await;

        let cached_db = request_cache.as_db_mut(StateProviderDatabase::new(&state_provider));
        let executor = self.evm_config.batch_executor(cached_db);

        let mut accessed_blacklisted = None;
        let output = executor.execute_with_state_closure(&block, |state| {
            if !self.disallow.is_empty() {
                // Check whether the submission interacted with any blacklisted account by scanning
                // the `State`'s cache that records everything read from database during execution.
                for account in state.cache.accounts.keys() {
                    if self.disallow.contains(account) {
                        accessed_blacklisted = Some(*account);
                    }
                }
            }
        })?;

        if let Some(account) = accessed_blacklisted {
            return Err(ValidationApiError::Blacklist(account));
        }

        // update the cached reads
        self.update_cached_reads(parent_header_hash, request_cache).await;

        self.consensus.validate_block_post_execution(&block, &output, None)?;

        self.ensure_payment(&block, &output, &message)?;

        let state_root =
            state_provider.state_root(state_provider.hashed_post_state(&output.state))?;

        if state_root != block.header().state_root() {
            return Err(ConsensusError::BodyStateRootDiff(
                GotExpected { got: state_root, expected: block.header().state_root() }.into(),
            )
            .into());
        }

        Ok(())
    }

    /// Ensures that fields of [`BidTrace`] match the fields of the [`SealedHeaderFor`].
    fn validate_message_against_header(
        &self,
        header: &SealedHeaderFor<E::Primitives>,
        message: &BidTrace,
    ) -> Result<(), ValidationApiError> {
        if header.hash() != message.block_hash {
            Err(ValidationApiError::BlockHashMismatch(GotExpected {
                got: message.block_hash,
                expected: header.hash(),
            }))
        } else if header.parent_hash() != message.parent_hash {
            Err(ValidationApiError::ParentHashMismatch(GotExpected {
                got: message.parent_hash,
                expected: header.parent_hash(),
            }))
        } else if header.gas_limit() != message.gas_limit {
            Err(ValidationApiError::GasLimitMismatch(GotExpected {
                got: message.gas_limit,
                expected: header.gas_limit(),
            }))
        } else if header.gas_used() != message.gas_used {
            Err(ValidationApiError::GasUsedMismatch(GotExpected {
                got: message.gas_used,
                expected: header.gas_used(),
            }))
        } else {
            Ok(())
        }
    }

    /// Ensures that the chosen gas limit is the closest possible value for the validator's
    /// registered gas limit.
    ///
    /// Ref: <https://github.com/flashbots/builder/blob/a742641e24df68bc2fc476199b012b0abce40ffe/core/blockchain.go#L2474-L2477>
    fn validate_gas_limit(
        &self,
        registered_gas_limit: u64,
        parent_header: &SealedHeaderFor<E::Primitives>,
        header: &SealedHeaderFor<E::Primitives>,
    ) -> Result<(), ValidationApiError> {
        let max_gas_limit =
            parent_header.gas_limit() + parent_header.gas_limit() / GAS_LIMIT_BOUND_DIVISOR - 1;
        let min_gas_limit =
            parent_header.gas_limit() - parent_header.gas_limit() / GAS_LIMIT_BOUND_DIVISOR + 1;

        let best_gas_limit =
            std::cmp::max(min_gas_limit, std::cmp::min(max_gas_limit, registered_gas_limit));

        if best_gas_limit != header.gas_limit() {
            return Err(ValidationApiError::GasLimitMismatch(GotExpected {
                got: header.gas_limit(),
                expected: best_gas_limit,
            }));
        }

        Ok(())
    }

    /// Ensures that the proposer has received [`BidTrace::value`] for this block.
    ///
    /// Firstly attempts to verify the payment by checking the state changes, otherwise falls back
    /// to checking the latest block transaction.
    fn ensure_payment(
        &self,
        block: &SealedBlock<<E::Primitives as NodePrimitives>::Block>,
        output: &BlockExecutionOutput<<E::Primitives as NodePrimitives>::Receipt>,
        message: &BidTrace,
    ) -> Result<(), ValidationApiError> {
        let (mut balance_before, balance_after) = if let Some(acc) =
            output.state.state.get(&message.proposer_fee_recipient)
        {
            let balance_before = acc.original_info.as_ref().map(|i| i.balance).unwrap_or_default();
            let balance_after = acc.info.as_ref().map(|i| i.balance).unwrap_or_default();

            (balance_before, balance_after)
        } else {
            // account might have balance but considering it zero is fine as long as we know
            // that balance have not changed
            (U256::ZERO, U256::ZERO)
        };

        if let Some(withdrawals) = block.body().withdrawals() {
            for withdrawal in withdrawals {
                if withdrawal.address == message.proposer_fee_recipient {
                    balance_before += withdrawal.amount_wei();
                }
            }
        }

        if balance_after >= balance_before.saturating_add(message.value) {
            return Ok(());
        }

        let (receipt, tx) = output
            .receipts
            .last()
            .zip(block.body().transactions().last())
            .ok_or(ValidationApiError::ProposerPayment)?;

        if !receipt.status() {
            return Err(ValidationApiError::ProposerPayment);
        }

        if tx.to() != Some(message.proposer_fee_recipient) {
            return Err(ValidationApiError::ProposerPayment);
        }

        if tx.value() != message.value {
            return Err(ValidationApiError::ProposerPayment);
        }

        if !tx.input().is_empty() {
            return Err(ValidationApiError::ProposerPayment);
        }

        if let Some(block_base_fee) = block.header().base_fee_per_gas() &&
            tx.effective_tip_per_gas(block_base_fee).unwrap_or_default() != 0
        {
            return Err(ValidationApiError::ProposerPayment);
        }

        Ok(())
    }

    /// Validates the given [`BlobsBundleV1`] and returns versioned hashes for blobs.
    ///
    /// This build has no KZG verifier, so any blob bundle is rejected.
    pub fn validate_blobs_bundle(
        &self,
        blobs_bundle: BlobsBundleV1,
    ) -> Result<Vec<B256>, ValidationApiError> {
        if blobs_bundle.commitments.len() != blobs_bundle.proofs.len() ||
            blobs_bundle.commitments.len() != blobs_bundle.blobs.len()
        {
            return Err(ValidationApiError::InvalidBlobsBundle);
        }

        Err(ValidationApiError::Blob(BlobTransactionValidationError::Unavailable))
    }

    /// Validates the given [`BlobsBundleV2`] and returns versioned hashes for blobs.
    ///
    /// This build has no KZG verifier, so any blob bundle is rejected.
    pub fn validate_blobs_bundle_v2(
        &self,
        _blobs_bundle: BlobsBundleV2,
    ) -> Result<Vec<B256>, ValidationApiError> {
        Err(ValidationApiError::Blob(BlobTransactionValidationError::Unavailable))
    }

    /// Core logic for validating the builder submission v3
    async fn validate_builder_submission_v3(
        &self,
        request: BuilderBlockValidationRequestV3,
    ) -> Result<(), ValidationApiError> {
        let block = self.payload_validator.ensure_well_formed_payload(ExecutionData {
            payload: ExecutionPayload::V3(request.request.execution_payload),
            sidecar: ExecutionPayloadSidecar::v3(CancunPayloadFields {
                parent_beacon_block_root: request.parent_beacon_block_root,
                versioned_hashes: self.validate_blobs_bundle(request.request.blobs_bundle)?,
            }),
        })?;

        self.validate_message_against_block(
            block,
            request.request.message,
            request.registered_gas_limit,
        )
        .await
    }

    /// Core logic for validating the builder submission v4
    async fn validate_builder_submission_v4(
        &self,
        request: BuilderBlockValidationRequestV4,
    ) -> Result<(), ValidationApiError> {
        let block = self.payload_validator.ensure_well_formed_payload(ExecutionData {
            payload: ExecutionPayload::V3(request.request.execution_payload),
            sidecar: ExecutionPayloadSidecar::v4(
                CancunPayloadFields {
                    parent_beacon_block_root: request.parent_beacon_block_root,
                    versioned_hashes: self.validate_blobs_bundle(request.request.blobs_bundle)?,
                },
                PraguePayloadFields {
                    requests: RequestsOrHash::Requests(execution_requests_to_requests(
                        &request.request.execution_requests,
                    )),
                },
            ),
        })?;

        self.validate_message_against_block(
            block,
            request.request.message,
            request.registered_gas_limit,
        )
        .await
    }

    /// Core logic for validating the builder submission v5
    async fn validate_builder_submission_v5(
        &self,
        request: BuilderBlockValidationRequestV5,
    ) -> Result<(), ValidationApiError> {
        let block = self.payload_validator.ensure_well_formed_payload(ExecutionData {
            payload: ExecutionPayload::V3(request.request.execution_payload),
            sidecar: ExecutionPayloadSidecar::v4(
                CancunPayloadFields {
                    parent_beacon_block_root: request.parent_beacon_block_root,
                    versioned_hashes: self
                        .validate_blobs_bundle_v2(request.request.blobs_bundle)?,
                },
                PraguePayloadFields {
                    requests: RequestsOrHash::Requests(execution_requests_to_requests(
                        &request.request.execution_requests,
                    )),
                },
            ),
        })?;

        // Check block size as per EIP-7934 (only applies when Osaka hardfork is active)
        let chain_spec = self.provider.chain_spec();
        if chain_spec.is_osaka_active_at_timestamp(block.timestamp()) &&
            block.rlp_length() > MAX_RLP_BLOCK_SIZE
        {
            return Err(ValidationApiError::Consensus(ConsensusError::BlockTooLarge {
                rlp_length: block.rlp_length(),
                max_rlp_length: MAX_RLP_BLOCK_SIZE,
            }));
        }

        self.validate_message_against_block(
            block,
            request.request.message,
            request.registered_gas_limit,
        )
        .await
    }
}

#[async_trait]
impl<Provider, E, T> BlockSubmissionValidationApiServer for ValidationApi<Provider, E, T>
where
    Provider: BlockReaderIdExt<Header = <E::Primitives as NodePrimitives>::BlockHeader>
        + ChainSpecProvider<ChainSpec: EthereumHardforks>
        + StateProviderFactory
        + Clone
        + 'static,
    E: ConfigureEvm + 'static,
    T: PayloadTypes<ExecutionData = ExecutionData>,
{
    async fn validate_builder_submission_v1(
        &self,
        _request: BuilderBlockValidationRequest,
    ) -> RpcResult<()> {
        warn!(target: "rpc::flashbots", "Method `flashbots_validateBuilderSubmissionV1` is not supported");
        Err(internal_rpc_err("unimplemented"))
    }

    async fn validate_builder_submission_v2(
        &self,
        _request: BuilderBlockValidationRequestV2,
    ) -> RpcResult<()> {
        warn!(target: "rpc::flashbots", "Method `flashbots_validateBuilderSubmissionV2` is not supported");
        Err(internal_rpc_err("unimplemented"))
    }

    /// Validates a block submitted to the relay
    async fn validate_builder_submission_v3(
        &self,
        request: BuilderBlockValidationRequestV3,
    ) -> RpcResult<()> {
        let this = self.clone();
        let (tx, rx) = oneshot::channel();

        self.task_spawner.spawn_blocking_task(async move {
            let result = Self::validate_builder_submission_v3(&this, request)
                .await
                .map_err(ErrorObject::from);
            let _ = tx.send(result);
        });

        rx.await.map_err(|_| internal_rpc_err("Internal blocking task error"))?
    }

    /// Validates a block submitted to the relay
    async fn validate_builder_submission_v4(
        &self,
        request: BuilderBlockValidationRequestV4,
    ) -> RpcResult<()> {
        let this = self.clone();
        let (tx, rx) = oneshot::channel();

        self.task_spawner.spawn_blocking_task(async move {
            let result = Self::validate_builder_submission_v4(&this, request)
                .await
                .map_err(ErrorObject::from);
            let _ = tx.send(result);
        });

        rx.await.map_err(|_| internal_rpc_err("Internal blocking task error"))?
    }

    /// Validates a block submitted to the relay
    async fn validate_builder_submission_v5(
        &self,
        request: BuilderBlockValidationRequestV5,
    ) -> RpcResult<()> {
        let this = self.clone();
        let (tx, rx) = oneshot::channel();

        self.task_spawner.spawn_blocking_task(async move {
            let result = Self::validate_builder_submission_v5(&this, request)
                .await
                .map_err(ErrorObject::from);
            let _ = tx.send(result);
        });

        rx.await.map_err(|_| internal_rpc_err("Internal blocking task error"))?
    }
}

pub struct ValidationApiInner<Provider, E: ConfigureEvm, T: PayloadTypes> {
    /// The provider that can interact with the chain.
    provider: Provider,
    /// Consensus implementation.
    consensus: Arc<dyn FullConsensus<E::Primitives>>,
    /// Execution payload validator.
    payload_validator:
        Arc<dyn PayloadValidator<T, Block = <E::Primitives as NodePrimitives>::Block>>,
    /// Block executor factory.
    evm_config: E,
    /// Set of disallowed addresses
    disallow: AddressSet,
    /// The maximum block distance - parent to latest - allowed for validation
    validation_window: u64,
    /// Cached state reads to avoid redundant disk I/O across multiple validation attempts
    /// targeting the same state. Stores a tuple of (`block_hash`, `cached_reads`) for the
    /// latest head block state. Uses async `RwLock` to safely handle concurrent validation
    /// requests.
    cached_state: RwLock<(B256, CachedReads)>,
    /// Task spawner for blocking operations
    task_spawner: Runtime,
    /// Validation metrics
    metrics: ValidationMetrics,
}

/// Calculates a deterministic hash of the blocklist for change detection.
///
/// This function sorts addresses to ensure deterministic output regardless of
/// insertion order, then computes a SHA256 hash of the concatenated addresses.
fn hash_disallow_list(disallow: &AddressSet) -> String {
    let mut sorted: Vec<_> = disallow.iter().collect();
    sorted.sort_unstable(); // sort for deterministic hashing

    let mut hasher = Sha256::new();
    for addr in sorted {
        hasher.update(addr.as_slice());
    }

    format!("{:x}", hasher.finalize())
}

impl<Provider, E: ConfigureEvm, T: PayloadTypes> fmt::Debug for ValidationApiInner<Provider, E, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidationApiInner").finish_non_exhaustive()
    }
}

/// Configuration for validation API.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ValidationApiConfig {
    /// Disallowed addresses.
    pub disallow: AddressSet,
    /// The maximum block distance - parent to latest - allowed for validation
    pub validation_window: u64,
}

impl ValidationApiConfig {
    /// Default validation blocks window of 3 blocks
    pub const DEFAULT_VALIDATION_WINDOW: u64 = 3;
}

impl Default for ValidationApiConfig {
    fn default() -> Self {
        Self { disallow: Default::default(), validation_window: Self::DEFAULT_VALIDATION_WINDOW }
    }
}

/// Errors thrown by the validation API.
#[derive(Debug, thiserror::Error)]
pub enum ValidationApiError {
    #[error("block gas limit mismatch: {_0}")]
    GasLimitMismatch(GotExpected<u64>),
    #[error("block gas used mismatch: {_0}")]
    GasUsedMismatch(GotExpected<u64>),
    #[error("block parent hash mismatch: {_0}")]
    ParentHashMismatch(GotExpected<B256>),
    #[error("block hash mismatch: {_0}")]
    BlockHashMismatch(GotExpected<B256>),
    #[error("missing latest block in database")]
    MissingLatestBlock,
    #[error("parent block not found")]
    MissingParentBlock,
    #[error("block is too old, outside validation window")]
    BlockTooOld,
    #[error("could not verify proposer payment")]
    ProposerPayment,
    #[error("invalid blobs bundle")]
    InvalidBlobsBundle,
    #[error("block accesses blacklisted address: {_0}")]
    Blacklist(Address),
    #[error(transparent)]
    Blob(#[from] BlobTransactionValidationError),
    #[error(transparent)]
    Consensus(#[from] ConsensusError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Execution(#[from] BlockExecutionError),
    #[error(transparent)]
    Payload(#[from] NewPayloadError),
}

impl From<ValidationApiError> for ErrorObject<'static> {
    fn from(error: ValidationApiError) -> Self {
        match error {
            ValidationApiError::GasLimitMismatch(_) |
            ValidationApiError::GasUsedMismatch(_) |
            ValidationApiError::ParentHashMismatch(_) |
            ValidationApiError::BlockHashMismatch(_) |
            ValidationApiError::Blacklist(_) |
            ValidationApiError::ProposerPayment |
            ValidationApiError::InvalidBlobsBundle |
            ValidationApiError::Blob(_) => invalid_params_rpc_err(error.to_string()),

            ValidationApiError::MissingLatestBlock |
            ValidationApiError::MissingParentBlock |
            ValidationApiError::BlockTooOld |
            ValidationApiError::Consensus(_) |
            ValidationApiError::Provider(_) => internal_rpc_err(error.to_string()),
            ValidationApiError::Execution(err) => match err {
                error @ BlockExecutionError::Validation(_) => {
                    invalid_params_rpc_err(error.to_string())
                }
                error @ BlockExecutionError::Internal(_) => internal_rpc_err(error.to_string()),
            },
            ValidationApiError::Payload(err) => match err {
                error @ NewPayloadError::Eth(_) => invalid_params_rpc_err(error.to_string()),
                error @ NewPayloadError::Other(_) => internal_rpc_err(error.to_string()),
            },
        }
    }
}

/// Metrics for the validation endpoint.
#[derive(Metrics)]
#[metrics(scope = "builder.validation")]
pub(crate) struct ValidationMetrics {
    /// The number of entries configured in the builder validation disallow list.
    pub(crate) disallow_size: Gauge,
}

#[cfg(test)]
mod tests {
    use super::{hash_disallow_list, AddressSet};
    use revm_primitives::Address;

    #[test]
    fn test_hash_disallow_list_deterministic() {
        let mut addresses = AddressSet::default();
        addresses.insert(Address::from([1u8; 20]));
        addresses.insert(Address::from([2u8; 20]));

        let hash1 = hash_disallow_list(&addresses);
        let hash2 = hash_disallow_list(&addresses);

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_hash_disallow_list_different_content() {
        let mut addresses1 = AddressSet::default();
        addresses1.insert(Address::from([1u8; 20]));

        let mut addresses2 = AddressSet::default();
        addresses2.insert(Address::from([2u8; 20]));

        let hash1 = hash_disallow_list(&addresses1);
        let hash2 = hash_disallow_list(&addresses2);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_hash_disallow_list_order_independent() {
        let mut addresses1 = AddressSet::default();
        addresses1.insert(Address::from([1u8; 20]));
        addresses1.insert(Address::from([2u8; 20]));

        let mut addresses2 = AddressSet::default();
        addresses2.insert(Address::from([2u8; 20])); // Different insertion order
        addresses2.insert(Address::from([1u8; 20]));

        let hash1 = hash_disallow_list(&addresses1);
        let hash2 = hash_disallow_list(&addresses2);

        assert_eq!(hash1, hash2);
    }

    #[test]
    //ensures parity with rbuilder hashing https://github.com/flashbots/rbuilder/blob/962c8444cdd490a216beda22c7eec164db9fc3ac/crates/rbuilder/src/live_builder/block_list_provider.rs#L248
    fn test_disallow_list_hash_rbuilder_parity() {
        let json = r#"["0x05E0b5B40B7b66098C2161A5EE11C5740A3A7C45","0x01e2919679362dFBC9ee1644Ba9C6da6D6245BB1","0x03893a7c7463AE47D46bc7f091665f1893656003","0x04DBA1194ee10112fE6C3207C0687DEf0e78baCf"]"#;
        let blocklist: Vec<Address> = serde_json::from_str(json).unwrap();
        let blocklist: AddressSet = blocklist.into_iter().collect();
        let expected_hash = "ee14e9d115e182f61871a5a385ab2f32ecf434f3b17bdbacc71044810d89e608";
        let hash = hash_disallow_list(&blocklist);
        assert_eq!(expected_hash, hash);
    }
}

#[cfg(test)]
mod execution_requests_tests {
    use super::*;
    use alloy_eips::{
        eip6110::DepositRequest, eip7002::WithdrawalRequest, eip7251::ConsolidationRequest,
    };
    use alloy_primitives::{hex, Bytes, FixedBytes};

    // SSZ test vectors for two Electra deposit, two withdrawal and one consolidation
    // request, taken from the `ssz_requests_conversions` tests of `alloy-rpc-types-beacon`
    // (originally from the execution-apis builder API examples).
    const DEPOSITS_HEX: &str = "0096a96086cff07df17668f35f7418ef8798079167e3f4f9b72ecde17b28226137cf454ab1dd20ef5d924786ab3483c2f9003f5102dabe0a27b1746098d1dc17a5d3fbd478759fea9287e4e419b3c3cef20100000000000000b1acdb2c4d3df3f1b8d3bfd33421660df358d84d78d16c4603551935f4b67643373e7eb63dcb16ec359be0ec41fee33b03a16e80745f2374ff1d3c352508ac5d857c6476d3c3bcf7e6ca37427c9209f17be3af5264c0e2132b3dd1156c28b4e9f000000000000000a5c85a60ba2905c215f6a12872e62b1ee037051364244043a5f639aa81b04a204c55e7cc851f29c7c183be253ea1510b001db70c485b6264692f26b8aeaab5b0c384180df8e2184a21a808a3ec8e86ca01000000000000009561731785b48cf1886412234531e4940064584463e96ac63a1a154320227e333fb51addc4a89b7e0d3f862d7c1fd4ea03bd8eb3d8806f1e7daf591cbbbb92b0beb74d13c01617f22c5026b4f9f9f294a8a7c32db895de3b01bee0132c9209e1f100000000000000";
    const WITHDRAWALS_HEX: &str = "01a94f5374fce5edbc8e2a8697c15331677e6ebf0b85103a5617937691dfeeb89b86a80d5dc9e3c9d3a1a0e7ce311e26e0bb732eabaa47ffa288f0d54de28209a62a7d29d0000000000000000000000000000000000000000000000000000010f698daeed734da114470da559bd4b4c7259e1f7952555241dcbc90cf194a2ef676fc6005f3672fada2a3645edb297a75530100000000000000";
    const CONSOLIDATIONS_HEX: &str = "02a94f5374fce5edbc8e2a8697c15331677e6ebf0b85103a5617937691dfeeb89b86a80d5dc9e3c9d3a1a0e7ce311e26e0bb732eabaa47ffa288f0d54de28209a62a7d29d098daeed734da114470da559bd4b4c7259e1f7952555241dcbc90cf194a2ef676fc6005f3672fada2a3645edb297a7553";

    fn bytes(hex_str: &str) -> Bytes {
        Bytes::from(hex::decode(hex_str).unwrap())
    }

    #[test]
    fn execution_requests_encoding_matches_ssz_vectors() {
        let deposits = bytes(DEPOSITS_HEX);
        let withdrawals = bytes(WITHDRAWALS_HEX);
        let consolidations = bytes(CONSOLIDATIONS_HEX);

        // All Electra request items are fixed-size (EIP-6110/7002/7251), so the payload
        // length pins the item sizes: deposit = 192 bytes, withdrawal = 76,
        // consolidation = 116.
        assert_eq!(deposits.len(), 1 + 2 * 192);
        assert_eq!(withdrawals.len(), 1 + 2 * 76);
        assert_eq!(consolidations.len(), 1 + 116);

        let deposit_items = deposits[1..]
            .as_chunks::<192>().0.iter()
            .map(|item| DepositRequest {
                pubkey: FixedBytes::from_slice(&item[0..48]),
                withdrawal_credentials: B256::from_slice(&item[48..80]),
                amount: u64::from_le_bytes(item[80..88].try_into().unwrap()),
                signature: FixedBytes::from_slice(&item[88..184]),
                index: u64::from_le_bytes(item[184..192].try_into().unwrap()),
            })
            .collect();

        let withdrawal_items = withdrawals[1..]
            .as_chunks::<76>().0.iter()
            .map(|item| WithdrawalRequest {
                source_address: Address::from_slice(&item[0..20]),
                validator_pubkey: FixedBytes::from_slice(&item[20..68]),
                amount: u64::from_le_bytes(item[68..76].try_into().unwrap()),
            })
            .collect();

        let consolidation_items = consolidations[1..]
            .as_chunks::<116>().0.iter()
            .map(|item| ConsolidationRequest {
                source_address: Address::from_slice(&item[0..20]),
                source_pubkey: FixedBytes::from_slice(&item[20..68]),
                target_pubkey: FixedBytes::from_slice(&item[68..116]),
            })
            .collect();

        let execution_requests = ExecutionRequestsV4 {
            deposits: deposit_items,
            withdrawals: withdrawal_items,
            consolidations: consolidation_items,
        };

        let requests = execution_requests_to_requests(&execution_requests);
        assert_eq!(requests, Requests::new(vec![deposits, withdrawals, consolidations]));
    }

    #[test]
    fn execution_requests_encoding_field_layout_and_empty_omission() {
        let execution_requests = ExecutionRequestsV4 {
            deposits: vec![DepositRequest {
                pubkey: FixedBytes([0xaa; 48]),
                withdrawal_credentials: B256::repeat_byte(0xbb),
                amount: 0x0102_0304_0506_0708,
                signature: FixedBytes([0xcc; 96]),
                index: 0x1122_3344_5566_7788,
            }],
            ..Default::default()
        };

        // pubkey || withdrawal_credentials || amount || signature || index, u64s LE.
        let mut expected = String::from("00");
        expected.push_str(&"aa".repeat(48));
        expected.push_str(&"bb".repeat(32));
        expected.push_str("0807060504030201");
        expected.push_str(&"cc".repeat(96));
        expected.push_str("8877665544332211");

        let requests = execution_requests_to_requests(&execution_requests);
        assert_eq!(requests, Requests::new(vec![bytes(&expected)]));

        // Empty request types must be omitted entirely.
        let requests = execution_requests_to_requests(&ExecutionRequestsV4::default());
        assert_eq!(requests, Requests::new(Vec::new()));
    }
}
