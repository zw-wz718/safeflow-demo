//! Fixed native adoption of member originals. This is no HTTP DTO or grant.
use crate::{
    PgRepository, PgSimulationSource, PgUnifiedFictionalSource, SimulationCapture,
    SimulationCaptureReader, SimulationSourceSpec, StorageError, UnifiedFictionalDocument,
    UNIFIED_FICTIONAL_COMMIT, UNIFIED_FICTIONAL_REVISION, UNIFIED_FICTIONAL_SOURCE,
};
use safeflow_billing_source_reader::{query::*, TrustedScope};
use safeflow_contracts::UserId;
use safeflow_subscription_evidence::managed_ledger_mock::LedgerFixture;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
mod calendar_next;
pub use calendar_next::{REVISION as TEAM_CALENDAR_NEXT_REVISION,SHA as TEAM_CALENDAR_NEXT_SHA,BYTES as TEAM_CALENDAR_NEXT_BYTES};

pub const TEAM_CONSUMPTION_SHA: &str =
    "086c5783ef1ab4cb8a2b8f139d1c43287cbdfe7b1fe22ea10af3f5696e4c948d";
pub const TEAM_INITIAL_SHA: &str =
    "01c218b5cbd865d2ea9c58ea10dea29879817fa74ed5a7fc2c79f67df6e5113d";
pub const TEAM_CONSUMPTION_REVISION: &str = "billing-team-consumption-proposed-20261027-v1";
pub const TEAM_AS_OF: &str = "2026-10-27T00:00:00+08:00";
pub const TEAM_ADOPTION_COMMIT: &str = "30505b0c071ca8f089af0c1017e8105c176f05a4";
const CONSUMPTION: &[u8] = include_bytes!(
    "../../../fixtures/team-consumption-initial/team_demo_consumption.proposed.v1.json"
);
const INITIAL: &[u8] =
    include_bytes!("../../../fixtures/team-consumption-initial/team_demo_initial_ledgers.v1.json");

#[derive(Deserialize)]
struct Consumption {
    source_id: String,
    revision: String,
    reference_user_account_commit: String,
    reference_source_revision: String,
    users: Vec<UserConsumption>,
    service_calendar: Value,
}
#[derive(Deserialize)]
struct UserConsumption {
    owner_user_id: String,
    request: Request,
    snapshot_id: String,
    pages: Vec<Step>,
}
#[derive(Deserialize)]
struct Step {
    cursor: Option<String>,
    page: Page,
}
#[derive(Deserialize)]
struct Initials {
    revision: String,
    reference_user_account_commit: String,
    reference_source_id: String,
    reference_source_revision: String,
    users: Vec<UserInitial>,
}
#[derive(Deserialize)]
struct UserInitial {
    owner_user_id: String,
    account_scope: Vec<String>,
    ledger: LedgerFixture,
}

/// Private typed originals: their metadata remains unchanged. Explicit native
/// adoption is independently bound to 30505b0 and the LF/Git original hashes.
pub struct TeamConsumptionDocument {
    data: Consumption,
    initial: Initials,
}
impl TeamConsumptionDocument {
    pub fn read_adopted(consumption: &[u8], initial: &[u8]) -> Result<Self, StorageError> {
        for (raw, expected) in [
            (consumption, TEAM_CONSUMPTION_SHA),
            (initial, TEAM_INITIAL_SHA),
        ] {
            if raw.len() > 64 * 1024 || format!("{:x}", Sha256::digest(raw)) != expected {
                return Err(StorageError::Invalid("team fixed original bytes"));
            }
        }
        let data: Consumption =
            serde_json::from_slice(consumption).map_err(|_| StorageError::CorruptRecord)?;
        let initial: Initials =
            serde_json::from_slice(initial).map_err(|_| StorageError::CorruptRecord)?;
        if data.source_id != UNIFIED_FICTIONAL_SOURCE
            || data.revision != TEAM_CONSUMPTION_REVISION
            || data.reference_user_account_commit != UNIFIED_FICTIONAL_COMMIT
            || data.reference_source_revision != UNIFIED_FICTIONAL_REVISION
            || initial.reference_user_account_commit != UNIFIED_FICTIONAL_COMMIT
            || initial.reference_source_id != UNIFIED_FICTIONAL_SOURCE
            || initial.reference_source_revision != UNIFIED_FICTIONAL_REVISION
            || initial.revision != "subscription-initial-empty-20261026-v1"
            || data.users.len() != 3
            || initial.users.len() != 3
        {
            return Err(StorageError::FactsConflict);
        }
        let doc = Self { data, initial };
        for owner in ["user_001", "user_002", "demo_user_li_v1"] {
            doc.input(&UserId::parse(owner).unwrap())?;
        }
        Ok(doc)
    }
    pub fn fixed_adopted() -> Result<Self, StorageError> {
        Self::read_adopted(CONSUMPTION, INITIAL)
    }
    pub fn provenance(&self) -> Value {
        json!({"adoption_commit":TEAM_ADOPTION_COMMIT,"adoption_sha256":"c224a40fcbac585220690eba8baa5a206045d42aab635be6ade27dc10e7d811b",
            "consumption_commit":"7b3e63390cdeb61631817eb1f14bea2d645ae613","consumption_sha256":TEAM_CONSUMPTION_SHA,
            "initial_commit":"deba42db1835e6cc906e3f064167c66185beead3","initial_sha256":TEAM_INITIAL_SHA,
            "identity_commit":UNIFIED_FICTIONAL_COMMIT,"identity_revision":UNIFIED_FICTIONAL_REVISION,
            "initial_revision":self.initial.revision,"calendar_revision":"billing-team-consumption-proposed-20261027-v1-service-calendar",
            "permissions_granted":false,"history_balance_effect":"NONE"})
    }
    fn input(
        &self,
        owner: &UserId,
    ) -> Result<(SimulationSourceSpec, Vec<Transaction>, LedgerFixture), StorageError> {
        let user = self
            .data
            .users
            .iter()
            .find(|u| u.owner_user_id == owner.as_str())
            .ok_or(StorageError::NotFound)?;
        let initial = self
            .initial
            .users
            .iter()
            .find(|u| u.owner_user_id == owner.as_str())
            .ok_or(StorageError::DependencyUnavailable)?;
        // Identity/account authority is the separately fixed 7c03 document, never this JSON.
        let identity = UnifiedFictionalDocument::read_adopted_v1(include_bytes!(
            "../../../../../fixtures/transfer/unified_demo_source_v1.json"
        ))?;
        let accounts: Vec<_> = identity
            .complete_accounts(owner)?
            .iter()
            .map(ToString::to_string)
            .collect();
        if accounts != user.request.account_ids
            || accounts != initial.account_scope
            || user.request.window.as_of != TEAM_AS_OF
            || local_four_month_window(TEAM_AS_OF).as_ref() != Some(&user.request.window)
        {
            return Err(StorageError::FactsConflict);
        }
        let mut rows = Vec::new();
        let mut cursor = None;
        for (i, step) in user.pages.iter().enumerate() {
            let p = &step.page;
            let last = i + 1 == user.pages.len();
            if step.cursor != cursor
                || p.owner_user_id != owner.as_str()
                || p.account_ids != accounts
                || p.window != user.request.window
                || p.snapshot_id != user.snapshot_id
                || p.has_more == last
                || p.range_complete != last
                || p.items.len() > 5
                || (!last && p.total_unique_rows.is_some())
            {
                return Err(StorageError::FactsConflict);
            }
            rows.extend(p.items.clone());
            cursor = p.next_cursor.clone();
            if last && (cursor.is_some() || p.total_unique_rows != Some(rows.len())) {
                return Err(StorageError::FactsConflict);
            }
        }
        let expected = match owner.as_str() {
            "user_001" => 21,
            "user_002" => 2,
            "demo_user_li_v1" => 0,
            _ => return Err(StorageError::NotFound),
        };
        if rows.len() != expected || user.pages.is_empty() {
            return Err(StorageError::FactsConflict);
        }
        Ok((
            SimulationSourceSpec {
                source_id: UNIFIED_FICTIONAL_SOURCE.into(),
                owner: owner.to_string(),
                revision: TEAM_CONSUMPTION_REVISION.into(),
                accounts,
                coverage: user.request.window.clone(),
                complete: true,
                coverage_evidence: serde_json::to_string(&self.provenance())
                    .map_err(|_| StorageError::CorruptRecord)?,
                page_size: 5,
            },
            rows,
            initial.ledger.clone(),
        ))
    }
}

/// Cannot be constructed from a client scope, Page or safe JSON. Its generic
/// source retains the expected fixed digest through every subsequent SQL verify.
#[derive(Clone)]
pub struct TeamConsumptionSource {
    source: PgSimulationSource,
    calendar: Value,
    next_calendar: Option<calendar_next::Version>,
}
impl TeamConsumptionSource {
    pub async fn import(
        identity: &PgUnifiedFictionalSource,
        owner: &UserId,
        document: &TeamConsumptionDocument,
    ) -> Result<Self, StorageError> {
        let (spec, rows, initial) = document.input(owner)?;
        Self::check_accounts(identity, owner, &spec).await?;
        let source = PgSimulationSource::import(
            identity.repository_for_isolated_backend(),
            spec,
            rows,
            Some(initial),
        )
        .await?;
        Self::bind(source, document, owner).await
    }
    pub async fn open(
        identity: &PgUnifiedFictionalSource,
        owner: &UserId,
    ) -> Result<Self, StorageError> {
        let doc = TeamConsumptionDocument::fixed_adopted()?;
        let (spec, _, _) = doc.input(owner)?;
        Self::check_accounts(identity, owner, &spec).await?;
        let source =
            PgSimulationSource::open_imported(identity.repository_for_isolated_backend(), spec)
                .await?;
        Self::bind(source, &doc, owner).await
    }
    async fn check_accounts(
        identity: &PgUnifiedFictionalSource,
        owner: &UserId,
        spec: &SimulationSourceSpec,
    ) -> Result<(), StorageError> {
        let actual: Vec<_> = identity
            .complete_accounts(owner)
            .await?
            .iter()
            .map(|a| a.account_id.to_string())
            .collect();
        if actual != spec.accounts {
            return Err(StorageError::FactsConflict);
        }
        Ok(())
    }
    async fn bind(
        mut source: PgSimulationSource,
        doc: &TeamConsumptionDocument,
        owner: &UserId,
    ) -> Result<Self, StorageError> {
        let (spec, mut rows, initial) = doc.input(owner)?;
        rows.sort_by_key(|r| (r.account_id.clone(), r.transaction_id.clone()));
        let hash = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(&spec, &rows, &Some(initial)))
                    .map_err(|_| StorageError::CorruptRecord)?
            )
        );
        source.bind_fixed_digest(hash).await?;
        let mut c=source.repository_for_backend().pool.acquire().await?;
        let next_calendar=calendar_next::Version::load(&mut c).await?;
        Ok(Self {
            source,
            calendar: doc.data.service_calendar.clone(),
            next_calendar,
        })
    }
    pub fn persistent_for_backend(&self) -> PgSimulationSource {
        self.source.clone()
    }
    pub fn request(&self) -> Request {
        Request {
            account_ids: self.source.spec().accounts.clone(),
            window: self.source.spec().coverage.clone(),
        }
    }
    pub fn owner(&self) -> &str {
        &self.source.spec().owner
    }
    pub fn calendar_for_backend(&self) -> Value {
        self.calendar_at(chrono::Utc::now())
    }
    pub(crate) fn calendar_at(&self, now: chrono::DateTime<chrono::Utc>) -> Value {
        // Safe owner-limited original calendar evidence, NOT a schedule/permit.
        let mut calendar = if let Some(next)=&self.next_calendar {next.select(&self.calendar,self.owner(),now)}else{self.calendar.clone()};
        if let Some(entries) = calendar.as_array_mut() {
            entries.retain(|e| e["owner_user_id"] == self.owner());
        }
        let mut adoption=TeamConsumptionDocument::fixed_adopted().unwrap().provenance();
        if let Some(next)=&self.next_calendar {adoption["approved_next_period"]=next.provenance();}
        json!({"evidence":calendar,"adoption":adoption,
            "confirmation_date_binding":"EVIDENCE_ONLY_SETTINGS_NOT_PREPARED","reminder_scheduled":false,"notification_sent":false})
    }
    pub(crate) fn calendar_binding_for_backend(&self)->Value {
        let mut old=self.calendar.clone();if let Some(entries)=old.as_array_mut(){entries.retain(|e|e["owner_user_id"]==self.owner());}
        json!({"original":old,"next":self.next_calendar.as_ref().map(|v|v.full(self.owner()))})
    }
    pub(crate) async fn check_calendar_on_connection(&self,c:&mut sqlx::PgConnection)->Result<(),StorageError>{
        let now=calendar_next::Version::load(c).await?;
        if now.is_some()!=self.next_calendar.is_some(){return Err(StorageError::FactsConflict);}
        Ok(())
    }
    pub(crate) fn reader(&self, capture: &SimulationCapture) -> TeamCaptureReader {
        TeamCaptureReader(capture.reader())
    }
    pub fn repository_for_backend(&self) -> PgRepository {
        self.source.repository_for_backend()
    }
}
/// No public constructor; only fixed-digest SQL source assembly issues this type.
pub(crate) struct TeamCaptureReader(SimulationCaptureReader);
impl PagedSource for TeamCaptureReader {
    fn source_id(&self) -> &str {
        self.0.source_id()
    }
    fn read_page(
        &self,
        s: &TrustedScope,
        r: &Request,
        c: Option<&str>,
        v: Option<&str>,
    ) -> Result<Page, PageError> {
        self.0.read_page(s, r, c, v)
    }
}

impl crate::SubscriptionService {
    /// Pure read review: no startup/host generation, Journal or preparation write.
    pub async fn identify_team_for_backend(
        &self,
        session: &safeflow_contracts::SessionRef,
        bank: &dyn safeflow_bank_gateway::BankGateway,
        source: &TeamConsumptionSource,
    ) -> Result<Value, StorageError> {
        tokio::time::timeout(crate::simulation_source::DEADLINE, async {
            let identity = self.identity(session)?;
            let request = source.request();
            let pg = source.persistent_for_backend();
            let sql = self
                .capture_simulation_for_backend(session, bank, &pg, &request)
                .await?;
            let scope = TrustedScope::from_verified_context(
                identity.user_id.as_str(),
                request.account_ids.iter().map(String::as_str),
            )
            .map_err(|_| StorageError::NotFound)?;
            let captured = crate::subscription_billing_reader::collect_from_adopted_team_capture(
                &scope,
                &request,
                &source.reader(&sql),
                Limits::default(),
            )
            .map_err(|_| StorageError::DependencyUnavailable)?;
            let mut tx = self.repository.subscription_read_pool.begin().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await?;
            // Verify this ledger connection is the SAME database/schema/current
            // generation as the original SQL capture; no cross-host assembly.
            sql.check_same_database(&mut tx).await?;
            let mut preview = captured
                .review_on_connection_mock(&self.store()?, &mut tx)
                .await
                .map_err(|_| StorageError::DependencyUnavailable)?;
            self.check(session, &identity)?;
            tx.commit().await?;
            self.check_simulation_accounts(session, bank, &identity, &request.account_ids)
                .await?;
            sql.check_live()?;
            preview["service_calendar_evidence"] = source.calendar_for_backend();
            Ok(preview)
        })
        .await
        .map_err(|_| StorageError::DependencyUnavailable)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_originals_scope_and_all_typed_rows() {
        let d = TeamConsumptionDocument::fixed_adopted().unwrap();
        let mut total = 0;
        for owner in ["user_001", "user_002", "demo_user_li_v1"] {
            let (_, rows, initial) = d.input(&UserId::parse(owner).unwrap()).unwrap();
            total += rows.len();
            assert_eq!(initial.owner_user_id, owner);
        }
        assert_eq!(total, 23);
        assert!(d.input(&UserId::parse("unknown").unwrap()).is_err());
        let mut changed = CONSUMPTION.to_vec();
        changed.push(b' ');
        assert!(TeamConsumptionDocument::read_adopted(&changed, INITIAL).is_err());
        assert!(TeamConsumptionDocument::read_adopted(CONSUMPTION, b"null").is_err());
    }
}
