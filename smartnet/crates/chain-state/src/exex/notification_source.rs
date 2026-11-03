/// Source of an execution-extension notification (retained for pipeline/stage API compatibility).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExExNotificationSource {
    /// Notification from the sync pipeline.
    Pipeline,
    /// Notification from the blockchain tree.
    BlockchainTree,
}
