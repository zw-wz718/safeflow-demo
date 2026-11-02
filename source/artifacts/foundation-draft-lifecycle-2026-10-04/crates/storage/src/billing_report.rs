//! Trusted source-page archive. Display JSON is never a report constructor.
use crate::models::hash;
use crate::{PgRepository, StorageError, TaskRecord};
use safeflow_billing_source_reader::{
    duplicates::detect_duplicates, query::*, spending::*, TrustedScope,
};
use safeflow_contracts::{ArgumentsHash, ToolResult, UserId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{types::Json, Postgres, Row, Transaction};
use std::sync::Mutex;

pub const BILLING_MEMBER_REVISION: &str = "8ff8c6657d724d4f69e7a3d69266b12ec148e0ea";
pub const BILLING_READ_TOOL: &str = "analyze_local_basic_spending";
const MAX_ARCHIVE: usize = 8 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageCall {
    cursor: Option<String>,
    snapshot: Option<String>,
    result: Result<Page, PageError>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive {
    owner: String,
    source_id: String,
    source_revision: String,
    member_revision: String,
    request: Request,
    metadata: ReportMetadata,
    pages: Vec<PageCall>,
    report: Value,
    duplicates: Value,
}
struct Capture<'a, S> {
    source: &'a S,
    calls: Mutex<Vec<PageCall>>,
}

pub(crate) fn original_source_identity(
    value: &Value,
) -> Result<(String, String, String), StorageError> {
    let archive: Archive =
        serde_json::from_value(value.clone()).map_err(|_| StorageError::CorruptRecord)?;
    Ok((
        archive.source_id,
        archive.source_revision,
        archive.member_revision,
    ))
}
impl<S: PagedSource> PagedSource for Capture<'_, S> {
    fn source_id(&self) -> &str {
        self.source.source_id()
    }
    fn read_page(
        &self,
        scope: &TrustedScope,
        request: &Request,
        cursor: Option<&str>,
        snapshot: Option<&str>,
    ) -> Result<Page, PageError> {
        let result = self.source.read_page(scope, request, cursor, snapshot);
        self.calls
            .lock()
            .map_err(|_| PageError::Unavailable)?
            .push(PageCall {
                cursor: cursor.map(str::to_owned),
                snapshot: snapshot.map(str::to_owned),
                result: result.clone(),
            });
        result
    }
}
struct Replay<'a> {
    archive: &'a Archive,
    index: Mutex<usize>,
}
impl PagedSource for Replay<'_> {
    fn source_id(&self) -> &str {
        &self.archive.source_id
    }
    fn read_page(
        &self,
        scope: &TrustedScope,
        request: &Request,
        cursor: Option<&str>,
        snapshot: Option<&str>,
    ) -> Result<Page, PageError> {
        if scope.user_id() != self.archive.owner || request != &self.archive.request {
            return Err(PageError::ResourceUnavailable);
        }
        let mut index = self.index.lock().map_err(|_| PageError::Unavailable)?;
        let call = self
            .archive
            .pages
            .get(*index)
            .ok_or(PageError::Unavailable)?;
        if call.cursor.as_deref() != cursor || call.snapshot.as_deref() != snapshot {
            return Err(PageError::SnapshotExpired);
        }
        *index += 1;
        call.result.clone()
    }
}

/// Called only by the adopted in-process source adapter. The returned attachment
/// goes through the sealed Runtime outcome, not an API/client-supplied payload.
pub fn capture_billing_report(
    scope: &TrustedScope,
    request: &Request,
    source: &impl PagedSource,
    metadata: &ReportMetadata,
    source_revision: &str,
) -> Result<(SpendingReport, Option<Value>), StorageError> {
    if source_revision.is_empty() || source_revision.len() > 128 {
        return Err(StorageError::DependencyUnavailable);
    }
    let capture = Capture {
        source,
        calls: Mutex::new(vec![]),
    };
    let report = analyze_spending(scope, request, &capture, Limits::default(), metadata)
        .map_err(|_| StorageError::Invalid("billing metadata"))?;
    // Failed/incomplete report displays remain readable, but cannot become selectable evidence.
    if !report.read.is_complete || report.calculation_status != CalculationStatus::Ready {
        return Ok((report, None));
    }
    let duplicates = serde_json::to_value(detect_duplicates(scope, &report))
        .map_err(|_| StorageError::CorruptRecord)?;
    let archive = Archive {
        owner: scope.user_id().into(),
        source_id: source.source_id().into(),
        source_revision: source_revision.into(),
        member_revision: BILLING_MEMBER_REVISION.into(),
        request: request.clone(),
        metadata: metadata.clone(),
        pages: capture
            .calls
            .into_inner()
            .map_err(|_| StorageError::CorruptRecord)?,
        report: serde_json::to_value(&report).map_err(|_| StorageError::CorruptRecord)?,
        duplicates,
    };
    let value = serde_json::to_value(archive).map_err(|_| StorageError::CorruptRecord)?;
    if serde_json::to_vec(&value)
        .map_err(|_| StorageError::CorruptRecord)?
        .len()
        > MAX_ARCHIVE
    {
        return Err(StorageError::Invalid("billing archive too large"));
    }
    Ok((report, Some(value)))
}

/// Monthly member algorithm over three separately captured real windows. Only
/// the original current typed report is archived for the existing feedback API.
pub fn capture_monthly_billing_report(
    scope: &TrustedScope, accounts: &[String], as_of: &str,
    source: &impl PagedSource, metadata: &ReportMetadata, source_revision: &str,
) -> Result<(safeflow_billing_source_reader::monthly_rules::MonthlyReport, Option<Value>), StorageError> {
    use safeflow_billing_source_reader::monthly_rules::{analyze_monthly, plan_monthly};
    let plan = plan_monthly(as_of).map_err(|_|StorageError::Invalid("monthly window"))?
        .ok_or(StorageError::Invalid("monthly window"))?;
    let current = Request { account_ids: accounts.to_vec(), window: plan.current };
    struct Current<'a,S> { source: &'a S, request: Request, calls: Mutex<Vec<PageCall>> }
    impl<S: PagedSource> PagedSource for Current<'_,S> {
        fn source_id(&self)->&str { self.source.source_id() }
        fn read_page(&self,s:&TrustedScope,r:&Request,c:Option<&str>,v:Option<&str>)->Result<Page,PageError>{
            let result=self.source.read_page(s,r,c,v);
            if r==&self.request { self.calls.lock().map_err(|_|PageError::Unavailable)?.push(PageCall{cursor:c.map(str::to_owned),snapshot:v.map(str::to_owned),result:result.clone()}); }
            result
        }
    }
    if source_revision.is_empty() || source_revision.len()>128 { return Err(StorageError::DependencyUnavailable); }
    let reader=Current{source,request:current.clone(),calls:Mutex::new(vec![])};
    let monthly=analyze_monthly(scope,accounts,as_of,&reader,Limits::default(),metadata)
        .map_err(|_|StorageError::Invalid("monthly metadata"))?;
    let report=monthly.current.as_ref().ok_or(StorageError::CorruptRecord)?;
    if !report.read.is_complete || report.calculation_status!=CalculationStatus::Ready { return Ok((monthly,None)); }
    let archive=Archive{owner:scope.user_id().into(),source_id:source.source_id().into(),source_revision:source_revision.into(),member_revision:BILLING_MEMBER_REVISION.into(),request:current,metadata:report.metadata.clone(),pages:reader.calls.into_inner().map_err(|_|StorageError::CorruptRecord)?,report:serde_json::to_value(report).map_err(|_|StorageError::CorruptRecord)?,duplicates:serde_json::to_value(detect_duplicates(scope,report)).map_err(|_|StorageError::CorruptRecord)?};
    let value=serde_json::to_value(archive).map_err(|_|StorageError::CorruptRecord)?;
    if serde_json::to_vec(&value).map_err(|_|StorageError::CorruptRecord)?.len()>MAX_ARCHIVE { return Err(StorageError::Invalid("billing archive too large")); }
    Ok((monthly,Some(value)))
}

pub(crate) struct RestoredReport {
    pub(crate) report: SpendingReport,
    pub(crate) archive_digest: ArgumentsHash,
    pub(crate) request: Request,
}
fn restore(archive: &Archive, scope: &TrustedScope) -> Result<SpendingReport, StorageError> {
    if archive.member_revision != BILLING_MEMBER_REVISION
        || archive.owner != scope.user_id()
        || archive
            .request
            .account_ids
            .iter()
            .any(|id| !scope.contains_account(id))
    {
        return Err(StorageError::NotFound);
    }
    let replay = Replay {
        archive,
        index: Mutex::new(0),
    };
    let report = analyze_spending(
        scope,
        &archive.request,
        &replay,
        Limits::default(),
        &archive.metadata,
    )
    .map_err(|_| StorageError::CorruptRecord)?;
    if *replay
        .index
        .lock()
        .map_err(|_| StorageError::CorruptRecord)?
        != archive.pages.len()
        || !report.read.is_complete
        || report.calculation_status != CalculationStatus::Ready
        || serde_json::to_value(&report).map_err(|_| StorageError::CorruptRecord)? != archive.report
        || serde_json::to_value(detect_duplicates(scope, &report))
            .map_err(|_| StorageError::CorruptRecord)?
            != archive.duplicates
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(report)
}

/// Only invoked INSIDE successful public read settlement, before its final guard/COMMIT.
pub(crate) async fn archive_settled_reports(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    claim: &str,
    kind: &str,
    receipt: &Value,
    results: &[ToolResult],
) -> Result<(), StorageError> {
    for result in results
        .iter()
        .filter(|r| r.tool_name.as_str() == BILLING_READ_TOOL)
    {
        let Some(value) = &result.backend_evidence else {
            continue;
        };
        let archive: Archive =
            serde_json::from_value(value.clone()).map_err(|_| StorageError::CorruptRecord)?;
        let scope = TrustedScope::from_verified_context(
            task.owner_id.as_str(),
            archive.request.account_ids.iter().map(String::as_str),
        )
        .map_err(|_| StorageError::CorruptRecord)?;
        restore(&archive, &scope)?;
        if archive.owner != task.owner_id.as_str()
            || result.data["adapter_contract"] != "LOCAL_BILLING_REPORT_ADAPTER_V2"
            || result.data["member_revision"] != BILLING_MEMBER_REVISION
            || result.data["report"] != archive.report
            || result.data["duplicate_candidates"] != archive.duplicates
            || result.data["source_revision"] != archive.source_revision
            || archive.metadata.report_id != if result.data.get("monthly_report").is_some() {
                format!("billing_report_{}:current",result.operation_id)
            } else { format!("billing_report_{}",result.operation_id) }
            || (result.data.get("monthly_report").is_some() &&
                (result.data["monthly_report"]["current"]!=archive.report ||
                 result.data["monthly_report"]["metadata"]["report_id"]!=format!("billing_report_{}",result.operation_id)))
            || result.status != safeflow_contracts::ToolResultStatus::Succeeded
        {
            return Err(StorageError::CorruptRecord);
        }
        sqlx::query("INSERT INTO public_billing_reports_v1(owner_id,report_id,task_id,read_operation_id,result_ref,claim_id,claim_kind,receipt_digest,result_digest,archive,archive_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(task.owner_id.as_str()).bind(&archive.metadata.report_id).bind(task.task_id.as_str())
            .bind(result.operation_id.as_str()).bind(result.result_ref.as_str()).bind(claim).bind(kind)
            .bind(hash(receipt)?.as_str()).bind(hash(&serde_json::json!(result))?.as_str()).bind(Json(value)).bind(hash(value)?.as_str())
            .execute(&mut **tx).await?;
    }
    Ok(())
}

pub(crate) fn visible_results(results: &[ToolResult]) -> Vec<ToolResult> {
    results
        .iter()
        .cloned()
        .map(|mut result| {
            result.backend_evidence = None;
            result
        })
        .collect()
}

/// Trusted SQL source only; no API accepts an Archive or a displayed report JSON.
pub(crate) async fn load_original(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    report_id: &str,
) -> Result<(Value, Vec<String>), StorageError> {
    let row =
        sqlx::query("SELECT * FROM public_billing_reports_v1 WHERE owner_id=$1 AND report_id=$2")
            .bind(owner.as_str())
            .bind(report_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(StorageError::NotFound)?;
    let value: Value = row.try_get("archive")?;
    if hash(&value)?.as_str() != row.try_get::<String, _>("archive_digest")? {
        return Err(StorageError::CorruptRecord);
    }
    let archive: Archive =
        serde_json::from_value(value.clone()).map_err(|_| StorageError::CorruptRecord)?;
    if archive.owner != owner.as_str() || archive.metadata.report_id != report_id {
        return Err(StorageError::CorruptRecord);
    }
    let task: String = row.try_get("task_id")?;
    let claim: String = row.try_get("claim_id")?;
    let kind: String = row.try_get("claim_kind")?;
    let sql = match kind.as_str() {
        "ROOT" => "SELECT receipt FROM runtime_read_claims WHERE task_id=$1 AND claim_id=$2 AND status='SETTLED'",
        "CONTINUATION" => "SELECT receipt FROM runtime_read_continuations WHERE task_id=$1 AND claim_id=$2 AND status='SETTLED'",
        _ => return Err(StorageError::CorruptRecord),
    };
    let receipt: Value = sqlx::query_scalar(sql)
        .bind(&task)
        .bind(&claim)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StorageError::CorruptRecord)?;
    if hash(&receipt)?.as_str() != row.try_get::<String, _>("receipt_digest")? {
        return Err(StorageError::CorruptRecord);
    }
    let results = receipt["results"]
        .as_array()
        .ok_or(StorageError::CorruptRecord)?;
    let operation: String = row.try_get("read_operation_id")?;
    let reference: String = row.try_get("result_ref")?;
    let result = results
        .iter()
        .find(|r| r["operation_id"] == operation && r["result_ref"] == reference)
        .ok_or(StorageError::CorruptRecord)?;
    if result["tool_name"] != BILLING_READ_TOOL
        || result["status"] != "SUCCEEDED"
        || hash(result)?.as_str() != row.try_get::<String, _>("result_digest")?
        || result["data"]["report"] != archive.report
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok((value, archive.request.account_ids))
}
pub(crate) fn restore_original(
    value: Value,
    scope: &TrustedScope,
) -> Result<RestoredReport, StorageError> {
    let archive: Archive =
        serde_json::from_value(value.clone()).map_err(|_| StorageError::CorruptRecord)?;
    Ok(RestoredReport {
        report: restore(&archive, scope)?,
        archive_digest: hash(&value)?,
        request: archive.request,
    })
}

impl PgRepository {
    /// Internal archive integrity read only. Current permissions are checked by
    /// BillingFeedbackService before it exposes or prepares the returned report.
    pub(crate) async fn original_billing_report(
        &self,
        owner: &UserId,
        id: &str,
    ) -> Result<(Value, Vec<String>), StorageError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let result = load_original(&mut tx, owner, id).await?;
        tx.commit().await?;
        Ok(result)
    }
}
