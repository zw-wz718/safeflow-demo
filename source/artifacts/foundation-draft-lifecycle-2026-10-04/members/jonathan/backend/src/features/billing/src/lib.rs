//! BILL-01 source-format reader. No funds commands, HTTP, authentication or C-TXN.
//!
//! The calling service supplies a verified identity/account scope; source adapters
//! independently enforce ownership and supply owner metadata. These Rust types
//! are not credentials. The offline fixture adapter is explicitly a simulation.

use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub mod duplicates;
pub mod feedback;
#[cfg(feature = "postgres")]
pub mod feedback_store;
pub mod monthly_rules;
pub mod query;
pub mod spending;
pub mod window_report;

pub const FIXTURE_SOURCE: &str = "transfer-mock:2a72b5d7ebfd7c77520b936333b2e13f8eccdcaf";
pub const DEMO_USER: &str = "user_demo";
pub const DEMO_ACCOUNT: &str = "account_001";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CnyAmount(String);

impl CnyAmount {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CnyAmount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let valid = value.split_once('.').is_some_and(|(whole, fraction)| {
            !whole.is_empty()
                && whole.bytes().all(|c| c.is_ascii_digit())
                && fraction.len() == 2
                && fraction.bytes().all(|c| c.is_ascii_digit())
                && whole
                    .parse::<u64>()
                    .ok()
                    .and_then(|n| n.checked_mul(100))
                    .and_then(|n| n.checked_add(fraction.parse().ok()?))
                    .is_some_and(|cents| cents > 0)
        });
        if !valid {
            return Err(D::Error::custom(
                "amount must be a positive two-decimal CNY string within u64 cents",
            ));
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Currency {
    CNY,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Direction {
    Debit,
    Credit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LedgerStatus {
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OperationStatus {
    Succeeded,
    Failed,
    Processing,
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    // Explicit deserializer makes an omitted Option field an error; null is valid.
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTransaction {
    pub transaction_id: String,
    pub operation_id: String,
    pub account_id: String,
    #[serde(deserialize_with = "required_nullable")]
    pub card_id: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub merchant_id: Option<String>,
    pub transaction_type: String,
    pub direction: Direction,
    pub amount: CnyAmount,
    pub currency: Currency,
    pub status: LedgerStatus,
    pub completed_at: String,
}

impl SourceTransaction {
    fn validate(&self) -> Result<(), BillingError> {
        for value in [
            &self.transaction_id,
            &self.operation_id,
            &self.account_id,
            &self.transaction_type,
            &self.completed_at,
        ] {
            validate_reference(value)?;
        }
        for value in [&self.card_id, &self.merchant_id].into_iter().flatten() {
            validate_reference(value)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationReceipt {
    pub proposal_version: u16,
    pub task_id: String,
    pub operation_id: String,
    pub draft_id: String,
    pub draft_version: u32,
    pub payer_account_id: String,
    pub payee_id: String,
    pub amount: CnyAmount,
    pub currency: Currency,
    pub status: OperationStatus,
    #[serde(deserialize_with = "required_nullable")]
    pub reason_code: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub completed_at: Option<String>,
}

impl OperationReceipt {
    fn validate(&self) -> Result<(), BillingError> {
        for value in [
            &self.task_id,
            &self.operation_id,
            &self.draft_id,
            &self.payer_account_id,
            &self.payee_id,
        ] {
            validate_reference(value)?;
        }
        for value in [&self.reason_code, &self.completed_at]
            .into_iter()
            .flatten()
        {
            validate_reference(value)?;
        }
        if self.proposal_version != 1
            || self.draft_version == 0
            || (self.status == OperationStatus::Succeeded) != self.completed_at.is_some()
        {
            return Err(BillingError::InvalidInput(
                "unsupported or inconsistent receipt".into(),
            ));
        }
        Ok(())
    }

    fn same_operation_parameters(&self, other: &Self) -> bool {
        self.proposal_version == other.proposal_version
            && self.task_id == other.task_id
            && self.operation_id == other.operation_id
            && self.draft_id == other.draft_id
            && self.draft_version == other.draft_version
            && self.payer_account_id == other.payer_account_id
            && self.payee_id == other.payee_id
            && self.amount == other.amount
            && self.currency == other.currency
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferFixture {
    pub proposal_version: u16,
    /// Untrusted fixture metadata. Never used to create a TrustedScope.
    pub authenticated_user_id: String,
    pub idempotency_key: String,
    pub clock_unix_seconds: u64,
    pub request: serde_json::Value,
    pub success: OperationReceipt,
    pub failure: OperationReceipt,
    pub processing: OperationReceipt,
    pub lost_reply: OperationReceipt,
    pub success_debit_transaction: SourceTransaction,
}

impl TransferFixture {
    pub fn from_json(input: &str) -> Result<Self, BillingError> {
        let fixture: Self = serde_json::from_str(input)
            .map_err(|error| BillingError::InvalidInput(error.to_string()))?;
        if fixture.proposal_version != 1 {
            return Err(BillingError::InvalidInput(
                "unsupported fixture proposal version".into(),
            ));
        }
        for receipt in [
            &fixture.success,
            &fixture.failure,
            &fixture.processing,
            &fixture.lost_reply,
        ] {
            receipt.validate()?;
        }
        fixture.success_debit_transaction.validate()?;
        let success = &fixture.success;
        let row = &fixture.success_debit_transaction;
        let same_parameters = [&fixture.failure, &fixture.processing, &fixture.lost_reply]
            .into_iter()
            .all(|receipt| success.same_operation_parameters(receipt));
        if !same_parameters
            || success.status != OperationStatus::Succeeded
            || fixture.failure.status != OperationStatus::Failed
            || fixture.processing.status != OperationStatus::Processing
            || fixture.lost_reply.status != OperationStatus::Processing
            || success.payer_account_id != DEMO_ACCOUNT
            || row.account_id != success.payer_account_id
            || row.operation_id != success.operation_id
            || row.amount != success.amount
            || row.currency != success.currency
            || Some(&row.completed_at) != success.completed_at.as_ref()
            || row.transaction_type != "TRANSFER"
            || row.direction != Direction::Debit
        {
            return Err(BillingError::InvalidInput(
                "inconsistent fixed transfer fixture".into(),
            ));
        }
        Ok(fixture)
    }
}

/// Construct only in a trusted service boundary after checking identity and
/// account ownership. This constructor does not perform authentication.
#[derive(Clone, Debug)]
pub struct TrustedScope {
    user_id: String,
    owned_account_ids: BTreeSet<String>,
}

impl TrustedScope {
    pub fn from_verified_context<'a>(
        user_id: &str,
        owned_account_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, BillingError> {
        validate_reference(user_id)?;
        let accounts: BTreeSet<String> = owned_account_ids.into_iter().map(str::to_owned).collect();
        for account in &accounts {
            validate_reference(account)?;
        }
        Ok(Self {
            user_id: user_id.into(),
            owned_account_ids: accounts,
        })
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn contains_account(&self, account_id: &str) -> bool {
        self.owned_account_ids.contains(account_id)
    }
}

#[derive(Clone, Debug)]
pub struct OwnedTransaction {
    /// Trusted adapter ownership metadata, not a field added to the source row.
    pub user_id: String,
    pub transaction: SourceTransaction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ObservedOperation {
    pub source_id: String,
    pub user_id: String,
    pub receipt: OperationReceipt,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SourceError {
    Unavailable,
    OperationNotFound,
    ResourceNotAccessible,
    InvalidResponse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BillingError {
    InvalidInput(String),
    Source(SourceError),
    ConflictingTransaction { transaction_id: String },
}

impl fmt::Display for BillingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(f, "invalid source input: {message}"),
            Self::Source(error) => write!(f, "source read failed: {error:?}"),
            Self::ConflictingTransaction { transaction_id } => {
                write!(f, "conflicting source transaction: {transaction_id}")
            }
        }
    }
}
impl std::error::Error for BillingError {}

fn validate_reference(value: &str) -> Result<(), BillingError> {
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(BillingError::InvalidInput(
            "empty or malformed source reference/time".into(),
        ));
    }
    Ok(())
}

/// Replaceable synchronous source boundary. Adapters must independently enforce
/// ownership; neither this interface nor fixture metadata is authentication.
/// There is intentionally no submit, execute, resolve or transfer method.
pub trait ReadOnlySource {
    fn source_id(&self) -> &str;
    fn read_completed_transactions(
        &self,
        scope: &TrustedScope,
    ) -> Result<Vec<OwnedTransaction>, SourceError>;
    fn query_operation(
        &self,
        scope: &TrustedScope,
        business_operation_id: &str,
    ) -> Result<ObservedOperation, SourceError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConsumptionAssessment {
    TransferExcluded,
    UnassessedSourceType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubscriptionEvidence {
    /// No four-month consumption source, pagination or completeness proof.
    Unavailable,
}

#[derive(Clone, Debug, Serialize)]
pub struct BillEntry {
    pub transaction: SourceTransaction,
    pub consumption: ConsumptionAssessment,
    pub card_note: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BillRead {
    pub source_id: String,
    /// All last-known rows in this reader for the current source/user/accounts.
    pub entries: Vec<BillEntry>,
    pub added: usize,
    pub unchanged: usize,
    pub excluded_out_of_scope: usize,
    pub subscription_evidence: SubscriptionEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TransactionKey {
    source_id: String,
    user_id: String,
    account_id: String,
    transaction_id: String,
}

/// In-memory, append-only last-known completed rows; not durable deduplication.
#[derive(Default)]
pub struct BillingReader {
    rows: BTreeMap<TransactionKey, SourceTransaction>,
}

impl BillingReader {
    pub fn read(
        &mut self,
        scope: &TrustedScope,
        source: &impl ReadOnlySource,
    ) -> Result<BillRead, BillingError> {
        validate_reference(source.source_id())?;
        let incoming = source
            .read_completed_transactions(scope)
            .map_err(BillingError::Source)?;
        // A conflicting or malformed batch must not partially mutate the cache.
        let mut staged = self.rows.clone();
        let (mut added, mut unchanged, mut excluded) = (0, 0, 0);
        for owned in incoming {
            if owned.user_id != scope.user_id
                || !scope.contains_account(&owned.transaction.account_id)
            {
                excluded += 1;
                continue;
            }
            owned.transaction.validate()?;
            let key = TransactionKey {
                source_id: source.source_id().into(),
                user_id: owned.user_id,
                account_id: owned.transaction.account_id.clone(),
                transaction_id: owned.transaction.transaction_id.clone(),
            };
            if let Some(previous) = staged.get(&key) {
                if previous != &owned.transaction {
                    return Err(BillingError::ConflictingTransaction {
                        transaction_id: key.transaction_id,
                    });
                }
                unchanged += 1;
            } else {
                staged.insert(key, owned.transaction);
                added += 1;
            }
        }
        let entries = staged
            .iter()
            .filter(|(key, _)| {
                key.source_id == source.source_id()
                    && key.user_id == scope.user_id
                    && scope.contains_account(&key.account_id)
            })
            .map(|(_, transaction)| BillEntry {
                consumption: if transaction.transaction_type == "TRANSFER" {
                    ConsumptionAssessment::TransferExcluded
                } else {
                    ConsumptionAssessment::UnassessedSourceType
                },
                card_note: transaction
                    .card_id
                    .is_none()
                    .then(|| "流水未提供对应卡片".into()),
                transaction: transaction.clone(),
            })
            .collect();
        self.rows = staged;
        Ok(BillRead {
            source_id: source.source_id().into(),
            entries,
            added,
            unchanged,
            excluded_out_of_scope: excluded,
            subscription_evidence: SubscriptionEvidence::Unavailable,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct QueryAdvice {
    pub action: String,
    pub business_operation_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct OperationView {
    pub last_known: ObservedOperation,
    pub query_error: Option<SourceError>,
    pub next_read: Option<QueryAdvice>,
}

fn operation_in_scope(
    scope: &TrustedScope,
    source_id: &str,
    operation: &ObservedOperation,
) -> bool {
    operation.source_id == source_id
        && operation.user_id == scope.user_id
        && scope.contains_account(&operation.receipt.payer_account_id)
        && operation.receipt.validate().is_ok()
}

/// Refresh by the ORIGINAL business ID. Read errors preserve the full last-known
/// receipt and surface the error separately; they never imply bank failure.
pub fn refresh_operation(
    scope: &TrustedScope,
    source: &impl ReadOnlySource,
    last_known: &ObservedOperation,
) -> Result<OperationView, BillingError> {
    if !operation_in_scope(scope, source.source_id(), last_known) {
        return Err(BillingError::Source(SourceError::ResourceNotAccessible));
    }
    let result = source.query_operation(scope, &last_known.receipt.operation_id);
    let mut view = OperationView {
        last_known: last_known.clone(),
        query_error: None,
        next_read: None,
    };
    match result {
        Ok(next) => {
            let compatible = operation_in_scope(scope, source.source_id(), &next)
                && last_known.receipt.same_operation_parameters(&next.receipt)
                && (last_known.receipt.status == OperationStatus::Processing
                    || last_known.receipt == next.receipt);
            if compatible {
                view.last_known = next;
            } else {
                view.query_error = Some(SourceError::InvalidResponse);
            }
        }
        Err(error) => view.query_error = Some(error),
    }
    if view.query_error.is_some() || view.last_known.receipt.status == OperationStatus::Processing {
        view.next_read = Some(QueryAdvice {
            action: "QUERY_ORIGINAL_BUSINESS_OPERATION".into(),
            business_operation_id: last_known.receipt.operation_id.clone(),
        });
    }
    Ok(view)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DemoCase {
    Success,
    Failure,
    Processing,
    LostReply,
}

/// Offline, fixed identities only. Does not consume fixture user metadata as an
/// authenticated identity and cannot be registered as a service/LLM adapter.
pub struct OfflineFixtureSource {
    fixture: TransferFixture,
    case: DemoCase,
}

impl OfflineFixtureSource {
    pub fn new(fixture: TransferFixture, case: DemoCase) -> Self {
        Self { fixture, case }
    }

    pub fn initial_operation(&self) -> ObservedOperation {
        let receipt = match self.case {
            DemoCase::Success => &self.fixture.success,
            DemoCase::Failure => &self.fixture.failure,
            DemoCase::Processing => &self.fixture.processing,
            DemoCase::LostReply => &self.fixture.lost_reply,
        };
        self.observe(receipt)
    }

    fn observe(&self, receipt: &OperationReceipt) -> ObservedOperation {
        ObservedOperation {
            source_id: FIXTURE_SOURCE.into(),
            user_id: DEMO_USER.into(),
            receipt: receipt.clone(),
        }
    }

    fn check_demo_scope(scope: &TrustedScope) -> Result<(), SourceError> {
        if scope.user_id() != DEMO_USER || !scope.contains_account(DEMO_ACCOUNT) {
            return Err(SourceError::ResourceNotAccessible);
        }
        Ok(())
    }
}

impl ReadOnlySource for OfflineFixtureSource {
    fn source_id(&self) -> &str {
        FIXTURE_SOURCE
    }

    fn read_completed_transactions(
        &self,
        scope: &TrustedScope,
    ) -> Result<Vec<OwnedTransaction>, SourceError> {
        Self::check_demo_scope(scope)?;
        match self.case {
            DemoCase::Success | DemoCase::LostReply => Ok(vec![OwnedTransaction {
                user_id: DEMO_USER.into(),
                transaction: self.fixture.success_debit_transaction.clone(),
            }]),
            DemoCase::Failure | DemoCase::Processing => Ok(vec![]),
        }
    }

    fn query_operation(
        &self,
        scope: &TrustedScope,
        business_operation_id: &str,
    ) -> Result<ObservedOperation, SourceError> {
        Self::check_demo_scope(scope)?;
        let initial = self.initial_operation();
        if initial.receipt.operation_id != business_operation_id {
            return Err(SourceError::OperationNotFound);
        }
        if self.case == DemoCase::LostReply {
            // Confirmed upstream: bank committed and only the reply was lost.
            // Reuse the supplied success receipt; never manufacture a receipt.
            Ok(self.observe(&self.fixture.success))
        } else {
            Ok(initial)
        }
    }
}
