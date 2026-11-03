//! Node add-ons. Depend on core [`NodeComponents`](crate::NodeComponents).

use smartnet_node_api::{FullNodeComponents, NodeAddOns};

use crate::hooks::NodeHooks;

/// Additional node extensions.
///
/// At this point we consider all necessary components defined.
pub struct AddOns<Node: FullNodeComponents, AddOns: NodeAddOns<Node>> {
    /// Additional `NodeHooks` that are called at specific points in the node's launch lifecycle.
    pub hooks: NodeHooks<Node, AddOns>,
    /// Additional captured addons.
    pub add_ons: AddOns,
}
