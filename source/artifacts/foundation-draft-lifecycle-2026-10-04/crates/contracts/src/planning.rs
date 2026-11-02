use crate::{NodeId, TaskId, ToolName};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const AGENT_PLAN_CONTRACT_VERSION: u16 = 1;
pub const PLANNING_RESULT_CONTRACT_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningResult {
    pub contract_version: u16,
    pub understanding: Understanding,
    pub plan: Option<AgentPlan>,
    pub clarification: Option<Clarification>,
}

impl PlanningResult {
    pub fn for_plan(understanding: Understanding, plan: AgentPlan) -> Self {
        Self {
            contract_version: PLANNING_RESULT_CONTRACT_VERSION,
            understanding,
            plan: Some(plan),
            clarification: None,
        }
    }

    pub fn validate(&self) -> Result<(), PlanContractError> {
        if self.contract_version != PLANNING_RESULT_CONTRACT_VERSION {
            return Err(PlanContractError::UnsupportedPlanningResultVersion(
                self.contract_version,
            ));
        }

        match (
            self.understanding.decision,
            self.plan.as_ref(),
            self.clarification.as_ref(),
        ) {
            (UnderstandingDecision::Pass, Some(plan), None) => plan.validate_envelope(),
            (UnderstandingDecision::Clarify, None, Some(clarification))
                if !clarification.question.trim().is_empty() =>
            {
                Ok(())
            }
            _ => Err(PlanContractError::InvalidPlanningResultCombination),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Understanding {
    pub intent: String,
    pub entities: Value,
    #[serde(default)]
    pub missing_fields: Vec<String>,
    #[serde(default)]
    pub context_refs: Vec<String>,
    pub decision: UnderstandingDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UnderstandingDecision {
    Pass,
    Clarify,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clarification {
    pub question: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPlan {
    pub contract_version: u16,
    pub task_id: TaskId,
    pub plan_version: u32,
    pub nodes: Vec<PlanNode>,
}

impl AgentPlan {
    pub fn new(
        task_id: TaskId,
        plan_version: u32,
        nodes: Vec<PlanNode>,
    ) -> Result<Self, PlanContractError> {
        if plan_version == 0 {
            return Err(PlanContractError::ZeroPlanVersion);
        }

        Ok(Self {
            contract_version: AGENT_PLAN_CONTRACT_VERSION,
            task_id,
            plan_version,
            nodes,
        })
    }

    pub fn validate_envelope(&self) -> Result<(), PlanContractError> {
        if self.contract_version != AGENT_PLAN_CONTRACT_VERSION {
            return Err(PlanContractError::UnsupportedContractVersion(
                self.contract_version,
            ));
        }
        if self.plan_version == 0 {
            return Err(PlanContractError::ZeroPlanVersion);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum PlanNode {
    Tool {
        id: NodeId,
        tool: ToolName,
        arguments: Value,
        #[serde(default)]
        depends_on: Vec<NodeId>,
    },
    UserDecision {
        id: NodeId,
        #[serde(default)]
        depends_on: Vec<NodeId>,
    },
}

impl PlanNode {
    pub fn id(&self) -> &NodeId {
        match self {
            Self::Tool { id, .. } | Self::UserDecision { id, .. } => id,
        }
    }

    pub fn depends_on(&self) -> &[NodeId] {
        match self {
            Self::Tool { depends_on, .. } | Self::UserDecision { depends_on, .. } => depends_on,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanContractError {
    ZeroPlanVersion,
    UnsupportedContractVersion(u16),
    UnsupportedPlanningResultVersion(u16),
    InvalidPlanningResultCombination,
    TaskMismatch,
    PlanVersionMismatch,
}

impl fmt::Display for PlanContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPlanVersion => formatter.write_str("plan_version must be greater than zero"),
            Self::UnsupportedContractVersion(version) => {
                write!(
                    formatter,
                    "unsupported agent plan contract version: {version}"
                )
            }
            Self::UnsupportedPlanningResultVersion(version) => write!(
                formatter,
                "unsupported planning result contract version: {version}"
            ),
            Self::InvalidPlanningResultCombination => formatter.write_str(
                "planning result decision, plan, and clarification combination is invalid",
            ),
            Self::TaskMismatch => {
                formatter.write_str("plan acceptance task_id does not match the plan")
            }
            Self::PlanVersionMismatch => {
                formatter.write_str("plan acceptance plan_version does not match the plan")
            }
        }
    }
}

impl std::error::Error for PlanContractError {}
