//! RTBC admission inspector — the hook side of `SC-FILTER.md` (Wave 3).
//!
//! One inspector, one LIFO journal-checkpoint stack, no revm modifications
//! (approved design §W0.2b; acceptance proven by the `rtbc_prototype` tests).
//!
//! - `create` (pre-exec): initcode size check → on reject return
//!   `CreateInitCodeSizeLimit` (frame never runs); on pass stash an extra
//!   [`JournalCheckpoint`] below the frame's own.
//! - `create_end` (post-constructor): runtime size + PUSH-aware `SELFDESTRUCT`
//!   scan on success → accept: commit the stash; reject: revert the stash
//!   (full rollback incl. revm's already-committed install) and halt with
//!   `CreateContractSizeLimit` (deposit-failure class, §4). Already-failed
//!   constructors: just close the stash (revm handled its own frame).
//!
//! The pre-hook's own reject also triggers `create_end` (same-frame, immediate) —
//! the `pre_rejected` flag keeps it from popping the enclosing create's stash.

use crate::rtbc::{check_initcode, check_runtime, RtbcPolicy, RtbcReject};
use alloy_primitives::{Address, Bytes};
use revm::{
    context::ContextTr,
    context_interface::{journaled_state::JournalCheckpoint, JournalTr},
    interpreter::{
        CreateInputs, CreateOutcome, Gas, InstructionResult, InterpreterResult, InterpreterTypes,
    },
    Inspector,
};

/// Deployment admission inspector — see module docs.
#[derive(Debug, Default)]
pub struct RtbcInspector {
    /// Admission policy (defaults mirror the chainspec constants — §7.4).
    policy: RtbcPolicy,
    /// LIFO stack of extra checkpoints, one per in-flight create frame.
    stack: Vec<JournalCheckpoint>,
    /// Set when the pre-hook rejected without stashing (see module docs).
    pre_rejected: bool,
    /// `(creator, created)` for every accepted install — the contract-metadata capture
    /// reads these after execution (the creator is the address that ran the create
    /// frame; it is not derivable from state). `[]` unless the wiring site asks.
    creations: Vec<(Address, Address)>,
}

impl RtbcInspector {
    /// Inspector with an explicit policy.
    pub const fn new(policy: RtbcPolicy) -> Self {
        Self { policy, stack: Vec::new(), pre_rejected: false, creations: Vec::new() }
    }

    /// The `(creator, created)` pairs recorded for accepted installs, draining the log.
    pub fn take_creations(&mut self) -> Vec<(Address, Address)> {
        core::mem::take(&mut self.creations)
    }

    fn reject(reject: RtbcReject, len: usize) {
        tracing::warn!(
            target: "smartnet::rtbc",
            ?reject,
            bytes = len,
            "RTBC deployment refused"
        );
    }
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for RtbcInspector {
    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let init_code = inputs.init_code();
        if let Err(reject) = check_initcode(init_code, &self.policy) {
            Self::reject(reject, init_code.len());
            self.pre_rejected = true;
            return Some(CreateOutcome {
                result: InterpreterResult {
                    result: InstructionResult::CreateInitCodeSizeLimit,
                    output: Bytes::new(),
                    gas: Gas::new(inputs.gas_limit()),
                },
                address: None,
            });
        }
        let cp = context.journal_mut().checkpoint();
        self.stack.push(cp);
        None
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        if self.pre_rejected {
            self.pre_rejected = false;
            return;
        }
        let Some(cp) = self.stack.pop() else { return };
        if outcome.result.result.is_ok() {
            match check_runtime(&outcome.result.output, &self.policy) {
                Ok(()) => {
                    // Accepted install — remember who created it (contract metadata).
                    if let Some(created) = outcome.address {
                        self.creations.push((inputs.caller(), created));
                    }
                    context.journal_mut().checkpoint_commit()
                }
                Err(reject) => {
                    Self::reject(reject, outcome.result.output.len());
                    context.journal_mut().checkpoint_revert(cp);
                    outcome.result.result = InstructionResult::CreateContractSizeLimit;
                }
            }
        } else {
            context.journal_mut().checkpoint_commit();
        }
    }
}

/// Shorthand for wiring sites: `…_and_inspector(db, env, rtbc())`.
pub fn rtbc() -> RtbcInspector {
    RtbcInspector::default()
}
