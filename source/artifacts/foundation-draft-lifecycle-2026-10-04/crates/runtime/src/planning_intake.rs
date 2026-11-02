//! Bounded proposal intake, never task storage, a safety decision or execution authority.
use crate::{
    validate_tool_policy_bindings, LlmGateway, PlanningContext, PlanningError,
    PlanningGatewaySource, PlanningInput, PreflightInputContract, Runtime, SessionBinding,
};
use safeflow_contracts::{
    PlanAcceptanceRequest, PlanNode, PlanningResult, SessionRef, TaskId, ToolDefinition,
    UnderstandingDecision, UserId, AGENT_TOOL_CONTRACT_VERSION,
};
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::{collections::HashSet, fmt, sync::Arc};

pub const PLANNING_INTAKE_CONTRACT_VERSION: u16 = 1;
const MAX_BYTES: usize = 65536;
const MAX_TEXT: usize = 4096;
const MAX_ID: usize = 200;
const MAX_DEPTH: usize = 16;
const MAX_VALUES: usize = 4096;
const MAX_NODES: usize = 256;

/// Only the intake service constructs this value. Its model text is still untrusted.
/// It is not persisted and Runtime must repeat all execution gates.
/// ```compile_fail
/// let forged: safeflow_runtime::AcceptedPlanningProposal =
///     serde_json::from_str("{}").unwrap();
/// ```
pub struct AcceptedPlanningProposal {
    result: PlanningResult,
    task_id: TaskId,
    plan_version: u32,
}
impl AcceptedPlanningProposal {
    pub fn contract_version(&self) -> u16 {
        PLANNING_INTAKE_CONTRACT_VERSION
    }
    pub fn planning_result(&self) -> &PlanningResult {
        &self.result
    }
    pub fn task_id(&self) -> &TaskId {
        &self.task_id
    }
    pub fn plan_version(&self) -> u32 {
        self.plan_version
    }
    pub fn into_planning_result(self) -> PlanningResult {
        self.result
    }
}

/// Backend configuration uses the same explicitly approved direct-input contracts as
/// preflight review. No new business schema, model connection or identity source is created.
pub struct PlanningIntakeService<'a> {
    runtime: &'a Runtime,
    gateway: Option<Arc<dyn LlmGateway>>,
    contracts: Vec<PreflightInputContract>,
    lifecycle: bool,
}

/// Process-local checked context for task creation, never an authorization or wire contract.
/// Only intake can capture the initial Runtime, identity and approved registration snapshot.
/// ```compile_fail
/// let claimed: safeflow_runtime::PlanningPersistenceReview<'_> =
///     serde_json::from_str("{}").unwrap();
/// ```
pub struct PlanningPersistenceReview<'a> {
    proposal: AcceptedPlanningProposal,
    runtime: &'a Runtime,
    session: SessionRef,
    binding: SessionBinding,
    definitions: Vec<ToolDefinition>,
    metadata: Value,
    gateway_source: PlanningGatewaySource,
}
impl PlanningPersistenceReview<'_> {
    pub fn planning_result(&self) -> &PlanningResult {
        self.proposal.planning_result()
    }
    pub fn owner(&self) -> &UserId {
        &self.binding.user_id
    }
    pub fn session_ref(&self) -> &SessionRef {
        &self.session
    }
    pub fn session_binding(&self) -> &SessionBinding {
        &self.binding
    }
    pub fn gateway_source(&self) -> PlanningGatewaySource {
        self.gateway_source
    }
    pub fn registration_hash(&self) -> safeflow_contracts::ArgumentsHash {
        crate::runtime::hash_arguments(&serde_json::json!({
            "definitions": self.definitions, "metadata": self.metadata
        }))
    }
}
impl<'a> PlanningIntakeService<'a> {
    pub fn approved_tools_for_backend(&self) -> Result<Vec<ToolDefinition>, PlanningIntakeError> {
        self.definitions()
    }

    pub fn for_registered_backend(
        runtime: &'a Runtime,
        gateway: Option<Arc<dyn LlmGateway>>,
        contracts: Vec<PreflightInputContract>,
    ) -> Result<Self, PlanningIntakeError> {
        if !contracts.is_empty() && contracts.iter().all(lifecycle_projection) {
            let mut service = Self::for_lifecycle_backend(runtime, contracts)?;
            service.gateway = gateway;
            Ok(service)
        } else {
            Self::for_trusted_backend(runtime, gateway, contracts)
        }
    }

    /// Current registration provenance only; no Kernel, Policy or Tool is evaluated.
    pub fn registration_hash_for_backend(
        &self,
    ) -> Result<safeflow_contracts::ArgumentsHash, PlanningIntakeError> {
        Ok(crate::runtime::hash_arguments(&serde_json::json!({
            "definitions": self.definitions()?, "metadata": self.metadata()?
        })))
    }

    pub fn for_trusted_backend(
        runtime: &'a Runtime,
        gateway: Option<Arc<dyn LlmGateway>>,
        contracts: Vec<PreflightInputContract>,
    ) -> Result<Self, PlanningIntakeError> {
        let service = Self {
            runtime,
            gateway,
            contracts,
            lifecycle: false,
        };
        service.definitions()?;
        Ok(service)
    }

    pub fn for_lifecycle_backend(
        runtime: &'a Runtime,
        contracts: Vec<PreflightInputContract>,
    ) -> Result<Self, PlanningIntakeError> {
        if contracts
            .iter()
            .any(|contract| !lifecycle_projection(contract))
        {
            return Err(PlanningIntakeError::InputContract);
        }
        let mut service = Self::for_trusted_backend(runtime, None, contracts)?;
        service.lifecycle = true;
        Ok(service)
    }

    /// Expected task/version come from the trusted backend, never from model JSON.
    /// This phase does not establish database task ownership or authorize a revision.
    pub async fn accept_json(
        &self,
        session: &SessionRef,
        task: &TaskId,
        version: u32,
        raw: &[u8],
    ) -> Result<AcceptedPlanningProposal, PlanningIntakeError> {
        Ok(self
            .accept_json_for_persistence(session, task, version, raw)
            .await?
            .proposal)
    }

    pub async fn accept_json_for_persistence(
        &self,
        session: &SessionRef,
        task: &TaskId,
        version: u32,
        raw: &[u8],
    ) -> Result<PlanningPersistenceReview<'a>, PlanningIntakeError> {
        let binding = self.begin(session, task, version)?;
        let definitions = self.definitions()?;
        let metadata = self.metadata()?;
        let value = parse_json(raw)?;
        let result =
            serde_json::from_value(value).map_err(|_| PlanningIntakeError::InvalidResult)?;
        let proposal = self
            .finish(session, task, version, &binding, &definitions, result)
            .await?;
        self.persistence_review(
            session,
            binding,
            definitions,
            metadata,
            proposal,
            PlanningGatewaySource::Unspecified,
        )
    }

    /// One Gateway call with user text, approved tools and backend task/version only.
    pub async fn plan(
        &self,
        session: &SessionRef,
        task: &TaskId,
        version: u32,
        user_input: &str,
    ) -> Result<AcceptedPlanningProposal, PlanningIntakeError> {
        Ok(self
            .plan_for_persistence(session, task, version, user_input)
            .await?
            .proposal)
    }

    pub async fn plan_for_persistence(
        &self,
        session: &SessionRef,
        task: &TaskId,
        version: u32,
        user_input: &str,
    ) -> Result<PlanningPersistenceReview<'a>, PlanningIntakeError> {
        let binding = self.begin(session, task, version)?;
        if user_input.trim().is_empty() || user_input.len() > MAX_TEXT {
            return Err(PlanningIntakeError::InvalidInput);
        }
        let definitions = self.definitions()?;
        let metadata = self.metadata()?;
        let input = PlanningInput {
            user_input: user_input.to_owned(),
            tool_definitions: definitions.clone(),
        };
        let gateway = self
            .gateway
            .as_ref()
            .ok_or(PlanningIntakeError::GatewayNotConfigured)?;
        let source = gateway.source();
        let context = PlanningContext {
            task_id: task.clone(),
            plan_version: version,
        };
        let result = gateway.generate_with_context(&input, &context).await;
        self.check_session(session, &binding)?;
        self.check_definitions(&definitions)?;
        if self.metadata()? != metadata {
            return Err(PlanningIntakeError::Registration);
        }
        let result = result.map_err(|error| match error {
            PlanningError::InvalidResult(_) => PlanningIntakeError::InvalidResult,
            PlanningError::Gateway(_) => PlanningIntakeError::GatewayFailed,
        })?;
        let proposal = self
            .finish(session, task, version, &binding, &definitions, result)
            .await?;
        self.persistence_review(session, binding, definitions, metadata, proposal, source)
    }

    fn metadata(&self) -> Result<Value, PlanningIntakeError> {
        let mut metadata = serde_json::json!({"runtime": self.runtime.preflight_source_metadata_for_backend().map_err(|_| PlanningIntakeError::Registration)?, "approved_inputs":self.contracts});
        if self.lifecycle {
            metadata["input_contract"] = serde_json::json!("bounded-direct-input-v2");
            metadata["nested_input_source_digest"] = serde_json::json!(format!(
                "{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(
                    include_str!("decision_input_v2.rs")
                        .replace("\r\n", "\n")
                        .as_bytes()
                )
            ));
        }
        Ok(metadata)
    }
    fn persistence_review(
        &self,
        session: &SessionRef,
        binding: SessionBinding,
        definitions: Vec<ToolDefinition>,
        metadata: Value,
        proposal: AcceptedPlanningProposal,
        gateway_source: PlanningGatewaySource,
    ) -> Result<PlanningPersistenceReview<'a>, PlanningIntakeError> {
        let review = PlanningPersistenceReview {
            proposal,
            runtime: self.runtime,
            session: session.clone(),
            binding,
            definitions,
            metadata,
            gateway_source,
        };
        self.check_persistence_review(&review)?;
        Ok(review)
    }
    /// Synchronous guard usable before/after database waits and immediately before COMMIT.
    /// Does not recapture identity, accept a client snapshot, or run Kernel/Policy/Tool.
    pub fn check_persistence_review(
        &self,
        review: &PlanningPersistenceReview<'_>,
    ) -> Result<(), PlanningIntakeError> {
        if !std::ptr::eq(self.runtime, review.runtime) {
            return Err(PlanningIntakeError::Registration);
        }
        self.check_session(&review.session, &review.binding)?;
        self.check_definitions(&review.definitions)?;
        if self.metadata()? != review.metadata {
            return Err(PlanningIntakeError::Registration);
        }
        self.check_session(&review.session, &review.binding)
    }

    fn begin(
        &self,
        session: &SessionRef,
        task: &TaskId,
        version: u32,
    ) -> Result<SessionBinding, PlanningIntakeError> {
        let binding = self
            .runtime
            .session_binding_for_backend(session)
            .map_err(|_| PlanningIntakeError::Session)?;
        if version == 0 || !identifier(task.as_str()) || !identifier(session.as_str()) {
            return Err(PlanningIntakeError::InvalidInput);
        }
        Ok(binding)
    }
    fn check_session(
        &self,
        session: &SessionRef,
        binding: &SessionBinding,
    ) -> Result<(), PlanningIntakeError> {
        if self
            .runtime
            .session_binding_for_backend(session)
            .map_err(|_| PlanningIntakeError::Session)?
            != *binding
        {
            return Err(PlanningIntakeError::Session);
        }
        Ok(())
    }
    fn definitions(&self) -> Result<Vec<ToolDefinition>, PlanningIntakeError> {
        validate_tool_policy_bindings(
            &self.runtime.tools,
            &self.runtime.policies,
            &self.runtime.catalog,
        )
        .map_err(|_| PlanningIntakeError::Registration)?;
        if self.contracts.len() > MAX_NODES {
            return Err(PlanningIntakeError::Registration);
        }
        let mut seen = HashSet::new();
        let mut definitions = Vec::new();
        for contract in &self.contracts {
            let definition = self
                .runtime
                .tools
                .get(&contract.tool_name)
                .ok_or(PlanningIntakeError::Registration)?
                .definition();
            let policy = self
                .runtime
                .policies
                .definition(&contract.tool_name)
                .ok_or(PlanningIntakeError::Registration)?;
            if !seen.insert(&contract.tool_name)
                || definition.name != contract.tool_name
                || definition.contract_version != AGENT_TOOL_CONTRACT_VERSION
                || definition.schema_version != contract.tool_schema_version
                || policy.policy_version != contract.policy_version
                || !identifier(definition.name.as_str())
                || !text(&definition.description, MAX_TEXT)
                || !identifier(&definition.action)
                || contract.projection_revision.is_empty()
                || contract.projection_revision.len() > 80
                || !contract
                    .projection_revision
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            {
                return Err(PlanningIntakeError::Registration);
            }
            bound_value(&definition.input_schema)?;
            if serde_json::to_vec(&definition.input_schema)
                .map_err(|_| PlanningIntakeError::Registration)?
                .len()
                > 16384
            {
                return Err(PlanningIntakeError::Registration);
            }
            definitions.push(definition);
        }
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        if serde_json::to_vec(&definitions)
            .map_err(|_| PlanningIntakeError::Registration)?
            .len()
            > MAX_BYTES
        {
            return Err(PlanningIntakeError::Registration);
        }
        Ok(definitions)
    }
    fn check_definitions(&self, definitions: &[ToolDefinition]) -> Result<(), PlanningIntakeError> {
        if self.definitions()? != definitions {
            return Err(PlanningIntakeError::Registration);
        }
        Ok(())
    }
    async fn finish(
        &self,
        session: &SessionRef,
        task: &TaskId,
        version: u32,
        binding: &SessionBinding,
        definitions: &[ToolDefinition],
        result: PlanningResult,
    ) -> Result<AcceptedPlanningProposal, PlanningIntakeError> {
        validate_result(&result)?;
        if let Some(plan) = &result.plan {
            if plan.task_id != *task || plan.plan_version != version {
                return Err(PlanningIntakeError::TaskBinding);
            }
            for node in &plan.nodes {
                if let PlanNode::Tool {
                    tool, arguments, ..
                } = node
                {
                    let definition = definitions
                        .iter()
                        .find(|d| d.name == *tool)
                        .ok_or(PlanningIntakeError::Registration)?;
                    if self.lifecycle {
                        crate::decision_input::validate_nested_object(
                            &definition.input_schema,
                            arguments,
                        )
                    } else {
                        crate::decision_input::validate_direct_object(
                            &definition.input_schema,
                            arguments,
                        )
                    }
                    .map_err(|_| PlanningIntakeError::InputContract)?;
                }
            }
            let review = self
                .runtime
                .security
                .validate_plan(
                    PlanAcceptanceRequest::new(plan.clone())
                        .map_err(|_| PlanningIntakeError::InvalidResult)?,
                )
                .await;
            self.check_session(session, binding)?;
            self.check_definitions(definitions)?;
            if !review
                .map_err(|_| PlanningIntakeError::InvalidResult)?
                .accepted
            {
                return Err(PlanningIntakeError::InvalidResult);
            }
        }
        self.check_definitions(definitions)?;
        self.check_session(session, binding)?;
        Ok(AcceptedPlanningProposal {
            result,
            task_id: task.clone(),
            plan_version: version,
        })
    }
}

fn lifecycle_projection(contract: &PreflightInputContract) -> bool {
    contract.projection_revision == "bounded-direct-input-v2"
        || (contract.tool_name.as_str() == safeflow_contracts::PAYMENT_INTENT_TOOL
            && contract.projection_revision == safeflow_contracts::PAYMENT_LOCATOR_PROJECTION)
}

fn identifier(value: &str) -> bool {
    text(value, MAX_ID) && value.trim() == value
}
fn text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn validate_result(result: &PlanningResult) -> Result<(), PlanningIntakeError> {
    // Bound borrowed fields before allocating a serialized copy of a typed Gateway response.
    let mut budget = Budget::default();
    let u = &result.understanding;
    if !text(&u.intent, MAX_ID)
        || !u.entities.is_object()
        || u.missing_fields.len() > 64
        || !u.context_refs.is_empty()
        || u.missing_fields.iter().any(|s| !identifier(s))
        || u.missing_fields.iter().collect::<HashSet<_>>().len() != u.missing_fields.len()
        || (u.decision == UnderstandingDecision::Pass && !u.missing_fields.is_empty())
    {
        return Err(PlanningIntakeError::InvalidResult);
    }
    budget
        .string(&u.intent, false)
        .map_err(|_| PlanningIntakeError::Limit)?;
    for field in &u.missing_fields {
        budget
            .string(field, false)
            .map_err(|_| PlanningIntakeError::Limit)?;
    }
    visit_value(&u.entities, 2, &mut budget).map_err(|_| PlanningIntakeError::Limit)?;
    if result
        .clarification
        .as_ref()
        .is_some_and(|c| !text(&c.question, MAX_TEXT))
    {
        return Err(PlanningIntakeError::InvalidResult);
    }
    if let Some(c) = &result.clarification {
        budget
            .string(&c.question, false)
            .map_err(|_| PlanningIntakeError::Limit)?;
    }
    if let Some(plan) = &result.plan {
        if !identifier(plan.task_id.as_str())
            || plan.nodes.is_empty()
            || plan.nodes.len() > MAX_NODES
        {
            return Err(PlanningIntakeError::InvalidResult);
        }
        budget
            .string(plan.task_id.as_str(), false)
            .map_err(|_| PlanningIntakeError::Limit)?;
        for node in &plan.nodes {
            if !identifier(node.id().as_str())
                || node.depends_on().len() > MAX_NODES
                || node.depends_on().iter().any(|d| !identifier(d.as_str()))
                || node.depends_on().iter().collect::<HashSet<_>>().len() != node.depends_on().len()
            {
                return Err(PlanningIntakeError::InvalidResult);
            }
            for id in std::iter::once(node.id()).chain(node.depends_on()) {
                budget
                    .string(id.as_str(), false)
                    .map_err(|_| PlanningIntakeError::Limit)?;
            }
            if let PlanNode::Tool {
                tool, arguments, ..
            } = node
            {
                if !identifier(tool.as_str()) {
                    return Err(PlanningIntakeError::InvalidResult);
                }
                budget
                    .string(tool.as_str(), false)
                    .map_err(|_| PlanningIntakeError::Limit)?;
                visit_value(arguments, 4, &mut budget).map_err(|_| PlanningIntakeError::Limit)?;
            }
        }
    }
    result
        .validate()
        .map_err(|_| PlanningIntakeError::InvalidResult)?;
    let value = serde_json::to_value(result).map_err(|_| PlanningIntakeError::InvalidResult)?;
    bound_value(&value)?;
    if serde_json::to_vec(&value)
        .map_err(|_| PlanningIntakeError::InvalidResult)?
        .len()
        > MAX_BYTES
    {
        return Err(PlanningIntakeError::Limit);
    }
    Ok(())
}

#[derive(Default)]
struct Budget {
    values: usize,
    bytes: usize,
    transport: bool,
}
impl Budget {
    fn value(&mut self, depth: usize) -> Result<(), ()> {
        self.values += 1;
        if depth > MAX_DEPTH || self.values > MAX_VALUES {
            return Err(());
        }
        Ok(())
    }
    fn string(&mut self, value: &str, key: bool) -> Result<(), ()> {
        self.bytes += value.len();
        if value.len()
            > if key {
                MAX_ID
            } else if self.transport {
                MAX_BYTES
            } else {
                MAX_TEXT
            }
            || (key && (!identifier(value)))
            || self.bytes > if self.transport { 262144 } else { MAX_BYTES }
        {
            return Err(());
        }
        Ok(())
    }
}
fn visit_value(v: &Value, depth: usize, b: &mut Budget) -> Result<(), ()> {
    b.value(depth)?;
    match v {
        Value::Object(m) => {
            if m.len() > MAX_NODES {
                return Err(());
            }
            for (k, v) in m {
                b.string(k, true)?;
                visit_value(v, depth + 1, b)?;
            }
        }
        Value::Array(a) => {
            if a.len() > MAX_NODES {
                return Err(());
            }
            for v in a {
                visit_value(v, depth + 1, b)?;
            }
        }
        Value::String(s) => b.string(s, false)?,
        Value::Number(n) if n.is_f64() => return Err(()),
        _ => {}
    }
    Ok(())
}
fn bound_value(value: &Value) -> Result<(), PlanningIntakeError> {
    visit_value(value, 0, &mut Budget::default()).map_err(|_| PlanningIntakeError::Limit)
}

// Serde performs JSON parsing; this visitor rejects duplicate decoded keys before
// they could be silently collapsed by Value, including arbitrary nested entities.
struct UniqueValue<'a> {
    depth: usize,
    budget: &'a mut Budget,
}
impl<'de> DeserializeSeed<'de> for UniqueValue<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        self.budget
            .value(self.depth)
            .map_err(|_| D::Error::custom("bounded JSON required"))?;
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for UniqueValue<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded unique-key JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Value, E> {
        if self.budget.transport {
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .ok_or_else(|| E::custom("finite number required"))
        } else {
            Err(E::custom("integer or decimal string required"))
        }
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Value, E> {
        self.budget
            .string(v, false)
            .map_err(|_| E::custom("bounded string required"))?;
        Ok(Value::String(v.to_owned()))
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Value, M::Error> {
        let mut result = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            self.budget
                .string(&key, true)
                .map_err(|_| M::Error::custom("bounded key required"))?;
            if result.len() >= MAX_NODES || result.contains_key(&key) {
                return Err(M::Error::custom("bounded unique keys required"));
            }
            let value = map.next_value_seed(UniqueValue {
                depth: self.depth + 1,
                budget: self.budget,
            })?;
            result.insert(key, value);
        }
        Ok(Value::Object(result))
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Value, S::Error> {
        let mut result = Vec::new();
        while let Some(value) = seq.next_element_seed(UniqueValue {
            depth: self.depth + 1,
            budget: self.budget,
        })? {
            if result.len() >= MAX_NODES {
                return Err(S::Error::custom("bounded array required"));
            }
            result.push(value);
        }
        Ok(Value::Array(result))
    }
}
pub(crate) fn parse_json(raw: &[u8]) -> Result<Value, PlanningIntakeError> {
    parse_bounded_json(raw, false)
}
/// Data parsing only: rejects decoded duplicate keys, floats and excessive nesting.
pub fn parse_bounded_input_json_for_backend(raw: &[u8]) -> Result<Value, PlanningIntakeError> {
    parse_json(raw)
}
pub(crate) fn parse_transport_json(raw: &[u8]) -> Result<Value, PlanningIntakeError> {
    parse_bounded_json(raw, true)
}
fn parse_bounded_json(raw: &[u8], transport: bool) -> Result<Value, PlanningIntakeError> {
    if raw.is_empty() || raw.len() > if transport { 262144 } else { MAX_BYTES } {
        return Err(PlanningIntakeError::Limit);
    }
    let mut de = serde_json::Deserializer::from_slice(raw);
    let value = UniqueValue {
        depth: 0,
        budget: &mut Budget {
            transport,
            ..Budget::default()
        },
    }
    .deserialize(&mut de)
    .map_err(|_| PlanningIntakeError::InvalidJson)?;
    de.end().map_err(|_| PlanningIntakeError::InvalidJson)?;
    Ok(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanningIntakeError {
    Session,
    InvalidInput,
    Limit,
    InvalidJson,
    InvalidResult,
    TaskBinding,
    Registration,
    InputContract,
    GatewayNotConfigured,
    GatewayFailed,
}
impl fmt::Display for PlanningIntakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Session => "PLANNING_INTAKE_SESSION_REJECTED",
            Self::InvalidInput => "PLANNING_INTAKE_INPUT_REJECTED",
            Self::Limit => "PLANNING_INTAKE_LIMIT_EXCEEDED",
            Self::InvalidJson => "PLANNING_INTAKE_JSON_REJECTED",
            Self::InvalidResult => "PLANNING_INTAKE_RESULT_REJECTED",
            Self::TaskBinding => "PLANNING_INTAKE_TASK_BINDING_MISMATCH",
            Self::Registration => "PLANNING_INTAKE_REGISTRATION_REJECTED",
            Self::InputContract => "PLANNING_INTAKE_DIRECT_ARGUMENTS_REJECTED",
            Self::GatewayNotConfigured => "PLANNING_INTAKE_GATEWAY_NOT_CONFIGURED",
            Self::GatewayFailed => "PLANNING_INTAKE_GATEWAY_FAILED",
        })
    }
}
impl std::error::Error for PlanningIntakeError {}

#[cfg(test)]
#[path = "planning_persistence_tests.rs"]
mod persistence_tests;
