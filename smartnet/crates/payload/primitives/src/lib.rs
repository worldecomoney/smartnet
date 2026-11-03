//! Abstractions for working with execution payloads.
//!
//! This crate provides types and traits for execution and building payloads. The payload builder
//! event types were merged in from the former `smartnet-payload-builder-primitives` crate.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use smartnet_primitives_traits::{NodePrimitives, SealedBlock};

mod error;
pub use error::{
    EngineObjectValidationError, InvalidPayloadAttributesError, NewPayloadError,
    PayloadBuilderError, VersionSpecificValidationError,
};

mod events;
pub use events::{Events, PayloadEvents};

mod traits;
pub use traits::{
    payload_id, BuildNextEnv, BuiltPayload, BuiltPayloadExecutedBlock, PayloadAttributes,
    PayloadAttributesBuilder,
};

mod payload;
pub use payload::{ExecutionPayload, PayloadOrAttributes};

/// Core trait that defines the associated types for working with execution payloads.
pub trait PayloadTypes: Send + Sync + Unpin + core::fmt::Debug + Clone + 'static {
    /// The format for execution payload data that can be processed and validated.
    ///
    /// This type represents the canonical format for block data that includes
    /// all necessary information for execution and validation.
    type ExecutionData: ExecutionPayload;
    /// The type representing a successfully built payload/block.
    type BuiltPayload: BuiltPayload + Clone + Unpin;

    /// Attributes that specify how a payload should be constructed.
    ///
    /// These attributes typically come from external sources (e.g., consensus layer over RPC such
    /// as the Engine API) and contain parameters like timestamp, fee recipient, and randomness.
    type PayloadAttributes: PayloadAttributes + Unpin;

    /// Converts a sealed block into the execution payload format.
    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> Self::ExecutionData;
}

/// A type that represents whether or not we are validating a payload or payload attributes.
///
/// This is used to ensure that the correct error code is returned when validating the payload or
/// payload attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageValidationKind {
    /// We are validating fields of a payload attributes.
    /// This corresponds to `engine_forkchoiceUpdated`.
    PayloadAttributes,
    /// We are validating fields of a payload.
    /// This corresponds to `engine_newPayload`.
    Payload,
    /// We are validating a built payload.
    /// This corresponds to `engine_getPayload`.
    GetPayload,
}

impl MessageValidationKind {
    /// Returns an `EngineObjectValidationError` based on the given
    /// `VersionSpecificValidationError` and the current validation kind.
    pub const fn to_error(
        self,
        error: VersionSpecificValidationError,
    ) -> EngineObjectValidationError {
        match self {
            // Both NewPayload and GetPayload errors are treated as generic Payload validation
            // errors
            Self::Payload | Self::GetPayload => EngineObjectValidationError::Payload(error),
            Self::PayloadAttributes => EngineObjectValidationError::PayloadAttributes(error),
        }
    }
}

/// Determines how we should choose the payload to return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PayloadKind {
    /// Returns the next best available payload (the earliest available payload).
    /// This does not wait for a real for pending job to finish if there's no best payload yet and
    /// is allowed to race various payload jobs (empty, pending best) against each other and
    /// returns whichever job finishes faster.
    ///
    /// This should be used when it's more important to return a valid payload as fast as possible.
    /// For example, the engine API timeout for `engine_getPayload` is 1s and clients should rather
    /// return an empty payload than indefinitely waiting for the pending payload job to finish and
    /// risk missing the deadline.
    #[default]
    Earliest,
    /// Only returns once we have at least one built payload.
    ///
    /// Compared to [`PayloadKind::Earliest`] this does not race an empty payload job against the
    /// already in progress one, and returns the best available built payload or awaits the job in
    /// progress.
    WaitForPending,
}
