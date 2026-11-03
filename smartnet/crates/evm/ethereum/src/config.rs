use revm::primitives::hardfork::SpecId;

/// The ONE Mars execution spec for this network: `LONDON` — the minimum spec that executes
/// both tx types (legacy type-0 + 1559 type-2) with the `SmartNet` fixed-fee era. Uniform at
/// block 0, every height, never fork/height/genesis-derived. (The OSAKA bump stays staged:
/// it rides along Shanghai/Cancun wire shapes the validator does not produce yet — see
/// `MARS.md` §7. Bump mechanics = change this constant + the validator shapes, E2E-gated.)
pub const fn revm_spec_by_timestamp_and_block_number() -> SpecId {
    SpecId::LONDON
}

/// Same constant for sealed-header execution contexts.
pub const fn revm_spec() -> SpecId {
    SpecId::LONDON
}
