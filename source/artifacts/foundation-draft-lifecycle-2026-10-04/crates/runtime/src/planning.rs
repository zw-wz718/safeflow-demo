use crate::ToolRegistry;
use safeflow_contracts::{ContractFuture, PlanningResult, TaskId, ToolDefinition};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq)]
pub struct PlanningInput {
    pub user_input: String,
    pub tool_definitions: Vec<ToolDefinition>,
}

/// Trusted backend identifiers only; never identity or execution authorization.
pub struct PlanningContext {
    pub(crate) task_id: TaskId,
    pub(crate) plan_version: u32,
}
impl PlanningContext {
    pub fn task_id(&self) -> &TaskId {
        &self.task_id
    }
    pub fn plan_version(&self) -> u32 {
        self.plan_version
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlanningGatewaySource {
    Unspecified,
    MockLlm,
    OpenaiCompatible,
}

pub trait LlmGateway: Send + Sync {
    fn generate<'a>(
        &'a self,
        input: &'a PlanningInput,
    ) -> ContractFuture<'a, Result<PlanningResult, PlanningError>>;

    fn generate_with_context<'a>(
        &'a self,
        input: &'a PlanningInput,
        _context: &'a PlanningContext,
    ) -> ContractFuture<'a, Result<PlanningResult, PlanningError>> {
        self.generate(input)
    }

    fn source(&self) -> PlanningGatewaySource {
        PlanningGatewaySource::Unspecified
    }
}

pub struct MockLlm {
    fixed_result: PlanningResult,
    call_count: AtomicUsize,
    last_input: Mutex<Option<PlanningInput>>,
}

impl MockLlm {
    pub fn new(fixed_result: PlanningResult) -> Self {
        Self {
            fixed_result,
            call_count: AtomicUsize::new(0),
            last_input: Mutex::new(None),
        }
    }

    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    pub fn last_input(&self) -> Result<Option<PlanningInput>, PlanningError> {
        self.last_input
            .lock()
            .map(|input| input.clone())
            .map_err(|_| PlanningError::Gateway("mock input recorder is unavailable".to_owned()))
    }
}

impl LlmGateway for MockLlm {
    fn source(&self) -> PlanningGatewaySource {
        PlanningGatewaySource::MockLlm
    }
    fn generate<'a>(
        &'a self,
        input: &'a PlanningInput,
    ) -> ContractFuture<'a, Result<PlanningResult, PlanningError>> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let result = self.fixed_result.clone();
        let recorded = self
            .last_input
            .lock()
            .map(|mut last_input| *last_input = Some(input.clone()))
            .map_err(|_| PlanningError::Gateway("mock input recorder is unavailable".to_owned()));
        Box::pin(async move {
            recorded?;
            Ok(result)
        })
    }
}

pub struct PlanningService {
    llm: Arc<dyn LlmGateway>,
    tools: Arc<ToolRegistry>,
}

impl PlanningService {
    pub fn new(llm: Arc<dyn LlmGateway>, tools: Arc<ToolRegistry>) -> Self {
        Self { llm, tools }
    }

    pub async fn plan(
        &self,
        user_input: impl Into<String>,
    ) -> Result<PlanningResult, PlanningError> {
        let input = PlanningInput {
            user_input: user_input.into(),
            tool_definitions: self.tools.definitions(),
        };
        let result = self.llm.generate(&input).await?;
        result
            .validate()
            .map_err(|error| PlanningError::InvalidResult(error.to_string()))?;
        Ok(result)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanningError {
    Gateway(String),
    InvalidResult(String),
}

impl fmt::Display for PlanningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gateway(message) => write!(formatter, "LLM gateway failed: {message}"),
            Self::InvalidResult(message) => write!(formatter, "invalid planning result: {message}"),
        }
    }
}

impl std::error::Error for PlanningError {}
