//! Operator-facing SmartNet CLI helpers (`init`, default node launch).
//!
//! Upstream maintenance commands (`db`, `stage`, `prune`, `p2p`, …) were removed in P1b.

pub mod common;
pub mod init_cmd;
/// Node launch trait used to start a `SmartNet` node.
pub mod launcher;
pub mod node;

pub use node::{
    expand_tilde, operator_datadir_args, operator_default_node_command, NoArgs, NodeCommand,
};
