//! Trusted persistence supplies verified progress; no client progress DTO is accepted.
use crate::{ReadOnlyPlanReview, Runtime, RuntimeError, VerifiedReadOnlyExecution};
use safeflow_contracts::{AgentPlan, NodeId, PlanNode, UserId};
use std::collections::HashSet;

pub const READ_ONLY_CONTINUATION_CONTRACT_VERSION: u16 = 1;

/// Backend adapter for a committed, audited continuation claim. Implementations belong
/// to trusted assembly, like SessionResolver; never deserialize a request into this source.
/// Storage's implementation has private fields and is constructed only after DB verification.
pub trait ReadOnlyContinuationSource: Send + Sync {
    fn plan(&self) -> &AgentPlan;
    fn owner(&self) -> &UserId;
    fn facts_version(&self) -> u64;
    fn completed_nodes(&self) -> &[NodeId];
    fn accepted_waiting_node(&self) -> &NodeId;
}

impl Runtime {
    /// Executes remaining nodes through the same executor. Source progress is trusted
    /// persistence evidence, not user input or financial confirmation.
    pub async fn continue_reviewed_read_only(
        &self,
        review: &ReadOnlyPlanReview,
        source: &dyn ReadOnlyContinuationSource,
    ) -> Result<VerifiedReadOnlyExecution, RuntimeError> {
        self.check_read_only_review(review)?;
        let completed = continuation_seed(review, source)?;
        self.execute_reviewed_read_only_seeded(review, Some(&completed))
            .await
    }
}

fn continuation_seed(
    review: &ReadOnlyPlanReview,
    source: &dyn ReadOnlyContinuationSource,
) -> Result<Vec<NodeId>, RuntimeError> {
    let fail = || RuntimeError::RecoveryReview("invalid trusted continuation checkpoint".into());
    if review.plan() != source.plan()
        || review.owner() != source.owner()
        || review.facts_version() != source.facts_version()
    {
        return Err(fail());
    }
    let mut completed: HashSet<_> = source.completed_nodes().iter().cloned().collect();
    if completed.len() != source.completed_nodes().len()
        || completed.contains(source.accepted_waiting_node())
    {
        return Err(fail());
    }
    for id in &completed {
        let node = review
            .plan
            .nodes
            .iter()
            .find(|node| node.id() == id)
            .ok_or_else(fail)?;
        if !node
            .depends_on()
            .iter()
            .all(|dependency| completed.contains(dependency))
        {
            return Err(fail());
        }
    }
    // Reconstruct the executor's deterministic prefix, including accepted earlier decisions.
    let mut prefix = HashSet::new();
    while prefix.len() < completed.len() {
        let next = review
            .plan
            .nodes
            .iter()
            .find(|node| {
                !prefix.contains(node.id())
                    && node
                        .depends_on()
                        .iter()
                        .all(|dependency| prefix.contains(dependency))
            })
            .ok_or_else(fail)?;
        if !completed.contains(next.id()) {
            return Err(fail());
        }
        prefix.insert(next.id().clone());
    }
    let waiting = review
        .plan
        .nodes
        .iter()
        .find(|node| node.id() == source.accepted_waiting_node())
        .ok_or_else(fail)?;
    let next = review
        .plan
        .nodes
        .iter()
        .find(|node| {
            !completed.contains(node.id())
                && node
                    .depends_on()
                    .iter()
                    .all(|dependency| completed.contains(dependency))
        })
        .ok_or_else(fail)?;
    if next.id() != waiting.id()
        || !matches!(waiting, PlanNode::UserDecision { .. })
        || !waiting
            .depends_on()
            .iter()
            .all(|dependency| completed.contains(dependency))
    {
        return Err(fail());
    }
    completed.insert(waiting.id().clone());
    Ok(review
        .plan
        .nodes
        .iter()
        .filter(|node| completed.contains(node.id()))
        .map(|node| node.id().clone())
        .collect())
}
