//! Local read-only adapter of Jonathan d822709; no default consumption source.
use crate::ReadOnlyToolRegistration;
use safeflow_bank_gateway::{BankGateway, GetAccountBalanceInput};
use safeflow_billing_source_reader::{
    duplicates::detect_duplicates, query::*, spending::*, TrustedScope,
};
use safeflow_contracts::*;
use safeflow_runtime::{MockDraftClock, PreflightInputContract, ToolSecurityProfile};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::Arc};

pub const BILLING_REPORT_TOOL: &str = "analyze_local_basic_spending";
const POLICY: &str = "local-billing-scope-policy-v1";
const ACTION: &str = "READ_LOCAL_BASIC_SPENDING";
const MEMBER: &str = safeflow_storage::BILLING_MEMBER_REVISION;
const MODE: &str = "LOCAL_BOUNDED_IN_PROCESS_V1";

/// Trusted startup code explicitly attests a reviewed, finite local reader.
/// Synchronous network/production readers are not supported by this adapter.
#[derive(Clone)]
pub struct LocalBillingSource {
    reader: Option<Arc<dyn PagedSource + Send + Sync>>,
    postgres: Option<safeflow_storage::PgSimulationSource>,
    source_id: String,
    revision: String,
    slots: Arc<tokio::sync::Semaphore>,
    monthly: bool,
}
impl LocalBillingSource {
    pub fn for_reviewed_local_bounded_reader(
        reader: Arc<dyn PagedSource + Send + Sync>,
        revision: impl Into<String>,
    ) -> Result<Self, &'static str> {
        let revision = revision.into();
        let source_id = reader.source_id().to_owned();
        if !valid_revision(&source_id)
            || source_id.chars().any(char::is_whitespace)
            || !valid_revision(&revision)
        {
            return Err("billing source identity/revision required");
        }
        Ok(Self {
            reader: Some(reader),
            postgres: None,
            source_id,
            revision,
            slots: Arc::new(tokio::sync::Semaphore::new(2)),
            monthly: false,
        })
    }
    fn validate(&self) -> Result<(), &'static str> {
        if self
            .reader
            .as_ref()
            .is_some_and(|r| r.source_id() != self.source_id)
        {
            Err("BILLING_SOURCE_CHANGED")
        } else {
            Ok(())
        }
    }
    /// Explicit native configuration. SQL is awaited before the finite reader;
    /// neither HTTP nor the model chooses the provider or its owner/revision.
    pub fn for_persistent_simulation(source: safeflow_storage::PgSimulationSource) -> Self {
        Self {
            source_id: source.spec().source_id.clone(),
            revision: source.spec().revision.clone(),
            reader: None,
            postgres: Some(source),
            slots: Arc::new(tokio::sync::Semaphore::new(2)),
            monthly: false,
        }
    }
}
fn valid_revision(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.trim() == value
        && !value.chars().any(char::is_control)
}
struct Clock;
impl MockDraftClock for Clock {
    fn now_seconds(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    input_version: u16,
    account_ids_json: String,
    start_inclusive: String,
    end_exclusive: String,
    timezone: String,
}
fn now(clock: &dyn MockDraftClock) -> Result<String, &'static str> {
    let seconds = i64::try_from(clock.now_seconds()).map_err(|_| "BILLING_CLOCK_UNAVAILABLE")?;
    let instant = chrono::DateTime::from_timestamp(seconds, 0)
        .filter(|_| seconds > 0)
        .ok_or("BILLING_CLOCK_UNAVAILABLE")?;
    Ok(instant
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .to_rfc3339())
}
fn request(arguments: Value, as_of: &str) -> Result<Request, &'static str> {
    let input: Input = serde_json::from_value(arguments).map_err(|_| "INVALID_BILLING_INPUT")?;
    if input.input_version != 1 || input.account_ids_json.len() > 2048 {
        return Err("INVALID_BILLING_INPUT");
    }
    let accounts: Vec<String> =
        serde_json::from_str(&input.account_ids_json).map_err(|_| "INVALID_BILLING_INPUT")?;
    if accounts.is_empty()
        || accounts.len() > 16
        || accounts.iter().collect::<BTreeSet<_>>().len() != accounts.len()
    {
        return Err("INVALID_BILLING_INPUT");
    }
    for id in &accounts {
        let account_id = ResourceId::parse(id).map_err(|_| "INVALID_BILLING_INPUT")?;
        GetAccountBalanceInput { account_id }
            .validate()
            .map_err(|_| "INVALID_BILLING_INPUT")?;
    }
    let parse = |value: &str| chrono::DateTime::parse_from_rfc3339(value).ok();
    let start = parse(&input.start_inclusive).ok_or("INVALID_BILLING_WINDOW")?;
    let end = parse(&input.end_exclusive).ok_or("INVALID_BILLING_WINDOW")?;
    if input.timezone != "Asia/Shanghai"
        || input.start_inclusive.len() > 64
        || input.end_exclusive.len() > 64
        || start >= end
        || Some(end) > parse(as_of)
    {
        return Err("INVALID_BILLING_WINDOW");
    }
    Ok(Request {
        account_ids: accounts,
        window: Window {
            as_of: as_of.into(),
            start_inclusive: input.start_inclusive,
            end_exclusive: input.end_exclusive,
            timezone: input.timezone,
        },
    })
}
#[derive(Clone)]
struct Adapter {
    name: ToolName,
    bank: Arc<dyn BankGateway>,
    source: LocalBillingSource,
    clock: Arc<dyn MockDraftClock>,
}
impl Adapter {
    async fn scope(&self, owner: &UserId, request: &Request) -> Result<TrustedScope, &'static str> {
        self.source.validate()?;
        for id in &request.account_ids {
            let id = ResourceId::parse(id).map_err(|_| "INVALID_BILLING_INPUT")?;
            // Gateway contract checks access for this session owner; no owner field exists
            // in its output. collect separately verifies every page's owner and accounts.
            let output = self
                .bank
                .get_account_balance(owner, &id)
                .await
                .map_err(|error| error.reason_code())?;
            output
                .validate_for(&id)
                .map_err(|error| error.reason_code())?;
        }
        self.source.validate()?;
        TrustedScope::from_verified_context(
            owner.as_str(),
            request.account_ids.iter().map(String::as_str),
        )
        .map_err(|_| "INVALID_BILLING_SCOPE")
    }
}
struct Reader(LocalBillingSource);
impl PagedSource for Reader {
    fn source_id(&self) -> &str {
        &self.0.source_id
    }
    fn read_page(
        &self,
        scope: &TrustedScope,
        request: &Request,
        cursor: Option<&str>,
        snapshot: Option<&str>,
    ) -> Result<Page, PageError> {
        self.0.validate().map_err(|_| PageError::Unavailable)?;
        let page = self
            .0
            .reader
            .as_ref()
            .ok_or(PageError::Unavailable)?
            .read_page(scope, request, cursor, snapshot)?;
        self.0.validate().map_err(|_| PageError::Unavailable)?;
        Ok(page)
    }
}
fn name() -> ToolName {
    ToolName::parse(BILLING_REPORT_TOOL).unwrap()
}
fn result_ref(operation: &OperationId) -> ToolResultRef {
    ToolResultRef::parse(format!("billing_result_{operation}")).unwrap()
}
fn tool_error(code: &str) -> ToolError {
    ToolError {
        reason_code: ReasonCode::parse(code).unwrap(),
        message: "local billing report unavailable or input rejected".into(),
    }
}
impl AgentTool for Adapter {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(name(), "Read local gross consumption, categories, top five, separate refunds and duplicate candidates for review only; no production consumption source", json!({
            "type":"object","properties":{
                "input_version":{"type":"integer","enum":[1]},
                "account_ids_json":{"type":"string","minLength":1,"maxLength":2048},
                "start_inclusive":{"type":"string","minLength":1,"maxLength":64},
                "end_exclusive":{"type":"string","minLength":1,"maxLength":64},
                "timezone":{"type":"string","enum":["Asia/Shanghai"]}
            },"required":["input_version","account_ids_json","start_inclusive","end_exclusive","timezone"],"additionalProperties":false
        }), 1, false, ACTION).unwrap()
    }
    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let action = async {
                if !matches!(authorization, ToolExecutionAuthorization::ReadOnly) {
                    return Err(tool_error("READ_ONLY_AUTHORIZATION_REQUIRED"));
                }
                let as_of = now(self.clock.as_ref()).map_err(tool_error)?;
                let request = request(arguments, &as_of).map_err(tool_error)?;
                let scope = self
                    .scope(&context.authenticated_user_id, &request)
                    .await
                    .map_err(tool_error)?;
                let slot = self
                    .source
                    .slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| tool_error("BILLING_SOURCE_BUSY"))?;
                if self.source.monthly {
                    return self.execute_monthly(context,scope,request,as_of,slot).await;
                }
                let sql_capture = if let Some(source) = &self.source.postgres {
                    Some(
                        source
                            .capture(&context.authenticated_user_id, &request)
                            .await
                            .map_err(|_| tool_error("BILLING_SOURCE_UNAVAILABLE"))?,
                    )
                } else {
                    None
                };
                let source = if let Some(sql) = &sql_capture {
                    LocalBillingSource::for_reviewed_local_bounded_reader(
                        Arc::new(sql.reader()),
                        self.source.revision.clone(),
                    )
                    .map_err(tool_error)?
                } else {
                    self.source.clone()
                };
                let final_request = request.clone();
                let metadata = ReportMetadata {
                    report_id: format!("billing_report_{}", context.operation_id),
                    calculated_at: as_of,
                };
                // Only finite local readers are admitted. Offload CPU work; dropping this
                // future discards its result, but cannot abort an already running sync reader.
                let (report, duplicates, backend_evidence) =
                    tokio::task::spawn_blocking(move || {
                        let _slot = slot;
                        let revision = source.revision.clone();
                        let (report, backend_evidence) = safeflow_storage::capture_billing_report(
                            &scope,
                            &request,
                            &Reader(source),
                            &metadata,
                            &revision,
                        )
                        .map_err(|_| SpendingError::InvalidMetadata)?;
                        // Use the same member-owned immutable evidence and trusted scope.
                        // Preserve Err and null pairs: neither means no duplicates.
                        let duplicates = detect_duplicates(&scope, &report);
                        Ok::<_, SpendingError>((report, duplicates, backend_evidence))
                    })
                    .await
                    .map_err(|_| tool_error("BILLING_SOURCE_UNAVAILABLE"))?
                    .map_err(|_| tool_error("INVALID_BILLING_METADATA"))?;
                self.source.validate().map_err(tool_error)?;
                // Source SHARE lease remains held through the last actual Gateway
                // await. Dropping/cancelling this request cannot publish its result.
                self.scope(&context.authenticated_user_id, &final_request)
                    .await
                    .map_err(tool_error)?;
                if let Some(sql) = &sql_capture {
                    sql.check_live()
                        .map_err(|_| tool_error("BILLING_SOURCE_EXPIRED"))?;
                }
                let mut result = ToolResult::new(
                    name(),
                    context.operation_id.clone(),
                    result_ref(&context.operation_id),
                    ToolResultStatus::Succeeded,
                    json!({"adapter_contract":"LOCAL_BILLING_REPORT_ADAPTER_V2", "source_revision":self.source.revision,
                    "member_revision":MEMBER,"source_mode":if sql_capture.is_some(){"LOCAL_PG_CAPTURE_V1"}else{MODE},"production_source_connected":false,
                    "report":report,"duplicate_candidates":duplicates}),
                );
                result.backend_evidence = backend_evidence;
                Ok(result)
            };
            if self.source.postgres.is_some() {
                tokio::time::timeout(std::time::Duration::from_secs(5), action)
                    .await
                    .map_err(|_| tool_error("BILLING_SOURCE_TIMEOUT"))?
            } else {
                action.await
            }
        })
    }
}
fn findings(error: Option<&str>) -> Vec<SecurityFinding> {
    error
        .into_iter()
        .map(|code| SecurityFinding {
            check_id: CheckId::parse("LOCAL-BILLING-SCOPE-01").unwrap(),
            policy_version: PolicyVersion::parse(POLICY).unwrap(),
            source: SourceName::parse("local_billing_scope").unwrap(),
            kind: FindingKind::HardDeny,
            reason_code: ReasonCode::parse(code).unwrap(),
            evidence_refs: vec![],
            observed_value: None,
            event_ids: vec![],
            dedup_key: None,
            window_start_ms: None,
            window_end_ms: None,
        })
        .collect()
}
impl DomainPolicy for Adapter {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }
    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let validation = async {
                if context.tool_name != self.name {
                    return Err("INVALID_BILLING_INPUT");
                }
                let as_of = now(self.clock.as_ref())?;
                let request = request(context.normalized_arguments.clone(), &as_of)?;
                self.scope(&context.authenticated_user_id, &request).await?;
                Ok::<_, &'static str>(())
            }
            .await;
            Ok(findings(validation.err()))
        })
    }
    fn verify_execution<'a>(
        &'a self,
        context: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let valid = context.tool_name == self.name
                && self.source.validate().is_ok()
                && matches!(&context.observation, ExecutionObservation::ResultReceived { tool_result_ref, .. } if *tool_result_ref == result_ref(&context.operation_id));
            Ok(findings((!valid).then_some("INVALID_BILLING_RESULT")))
        })
    }
}
/// Explicit local read-only approval, never invoked by default service startup.
/// The Gateway must be the same trusted provider passed to ApiState startup.
pub fn billing_report_registration_for_backend(
    bank: Arc<dyn BankGateway>,
    source: Option<LocalBillingSource>,
) -> Result<ReadOnlyToolRegistration, &'static str> {
    registration(bank, source, Arc::new(Clock))
}
/// Explicit fixed simulation clock, only for the fixed adopted team source.
pub(crate) fn team_billing_registration(
    bank: Arc<dyn BankGateway>,
    source: &safeflow_storage::TeamConsumptionSource,
) -> Result<ReadOnlyToolRegistration, &'static str> {
    struct Fixed;
    impl MockDraftClock for Fixed {
        fn now_seconds(&self) -> u64 {
            chrono::DateTime::parse_from_rfc3339(safeflow_storage::TEAM_AS_OF)
                .unwrap()
                .timestamp() as u64
        }
    }
    let mut local=LocalBillingSource::for_persistent_simulation(source.persistent_for_backend());
    local.monthly=true;
    registration_with_clock(
        bank,
        Some(local),
        Arc::new(Fixed),
        "fixed-team-as-of-20261027-v1",
    )
}
fn registration(
    bank: Arc<dyn BankGateway>,
    source: Option<LocalBillingSource>,
    clock: Arc<dyn MockDraftClock>,
) -> Result<ReadOnlyToolRegistration, &'static str> {
    registration_with_clock(bank, source, clock, "trusted-server-unix-seconds-v1")
}
fn registration_with_clock(
    bank: Arc<dyn BankGateway>,
    source: Option<LocalBillingSource>,
    clock: Arc<dyn MockDraftClock>,
    clock_revision: &str,
) -> Result<ReadOnlyToolRegistration, &'static str> {
    let source = source.ok_or("billing consumption source not provided")?;
    source.validate()?;
    let provenance = json!({"adapter":"local-billing-report-adapter-v2","member":MEMBER,
        "source_id":source.source_id,"source_revision":source.revision,"source_mode":if source.postgres.is_some(){"LOCAL_PG_CAPTURE_V1"}else{MODE},
        "persistent_source":source.postgres.as_ref().map(|s|s.provenance_for_backend()),
        "persistent_adapter_digest":source.postgres.as_ref().map(|_|format!("{:x}",Sha256::digest(include_str!("../../storage/src/simulation_source.rs").replace("\r\n","\n").as_bytes()))),
        "projection":if source.monthly {"monthly-three-captures-v1"}else{"flat-query-v1"},"clock":clock_revision,
        "limits":{"max_pages":100,"max_rows":10000},
        "member_digests":[
            format!("{:x}",Sha256::digest(include_bytes!("../../../members/jonathan/backend/src/features/billing/src/lib.rs"))),
            format!("{:x}",Sha256::digest(include_bytes!("../../../members/jonathan/backend/src/features/billing/src/query.rs"))),
            format!("{:x}",Sha256::digest(include_bytes!("../../../members/jonathan/backend/src/features/billing/src/spending.rs"))),
            format!("{:x}",Sha256::digest(include_bytes!("../../../members/jonathan/backend/src/features/billing/src/duplicates.rs")))],
        "adapter_digest":format!("{:x}",Sha256::digest(include_str!("billing.rs").replace("\r\n","\n").as_bytes()))});
    let revision = format!(
        "local-billing-v2:{:x}",
        Sha256::digest(serde_json::to_vec(&provenance).unwrap())
    );
    let adapter = Arc::new(Adapter {
        name: name(),
        bank,
        source,
        clock,
    });
    let policy = PolicyVersion::parse(POLICY).unwrap();
    Ok(ReadOnlyToolRegistration {
        tool: adapter.clone(),
        policy: adapter,
        policy_version: policy.clone(),
        compatible_schema_version: 1,
        profile: ToolSecurityProfile {
            tool_name: name(),
            tool_schema_version: 1,
            policy_version: policy.clone(),
            minimum_level: SecurityLevel::Green,
        },
        input: PreflightInputContract {
            tool_name: name(),
            tool_schema_version: 1,
            policy_version: policy,
            projection_revision: "bounded-direct-input-v2".into(),
        },
        approved_action: ACTION.into(),
        implementation_revision: revision,
    })
}

#[cfg(test)]
pub(crate) mod duplicate_tests;
#[cfg(test)]
pub(crate) mod tests;

mod monthly;
