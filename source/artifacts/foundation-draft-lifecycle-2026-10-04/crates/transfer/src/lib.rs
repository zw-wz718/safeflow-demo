//! Reusable bank/draft integration, plus opt-in Mock Runtime adapters in runtime_tools.

use safeflow_bank_gateway::{
    BankError, BankGateway, CnyCurrency, DecimalAmount, FindPayeesInput, FindPayeesOutput,
    GetAccountBalanceInput, GetAccountBalanceOutput, MaskedAccountView, PayeeView,
};
use safeflow_contracts::{ExecutionContext, OperationId, ResourceId, TaskId, UserId};
use safeflow_transfer as legacy;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[cfg(feature = "postgres-store")]
pub mod business_store;
pub mod daily_usage;
pub mod funds_policy;
#[cfg(feature = "postgres-store")]
pub mod funds_store;
pub mod mock_execution;
pub mod runtime_tools;
pub mod shared_demo;

pub use legacy::{Clock, DraftError, DraftStatus, SystemClock};

#[derive(Clone, Debug)]
pub enum PayeeSelection {
    Name(String),
    Id(ResourceId),
}

/// Business input only. Identity/task/time never come from these fields.
/// None retains a field. An ID selection is checked against a fresh search.
#[derive(Default)]
pub struct DraftInput {
    pub payer_account_id: Option<ResourceId>,
    pub payee_query: Option<String>,
    pub payee: Option<PayeeSelection>,
    pub amount: Option<String>,
}

#[derive(Default)]
pub struct DraftChanges {
    pub input: DraftInput,
    pub currency: Option<String>,
    pub remark: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftDetails {
    pub account: MaskedAccountView,
    pub payee: PayeeView,
    pub amount: DecimalAmount,
    pub currency: CnyCurrency,
    pub remark: String,
    /// Display snapshot only, not balance reservation or authorization.
    pub available_balance: DecimalAmount,
    pub balance_as_of: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDraft {
    pub draft_id: ResourceId,
    pub operation_id: OperationId,
    pub task_id: TaskId,
    pub user_id: UserId,
    pub plan_version: u32,
    pub status: DraftStatus,
    pub expires_at: u64,
    pub details: Option<DraftDetails>,
}

/// Backend-only recovery input, deliberately NOT deserializable as an HTTP/Tool
/// argument. The caller must read the CURRENT SQL row (including submitted) and
/// verify its ownership/task/version under the foundation's lifecycle locking.
/// payee_query is a trusted retained/new explicit search, never a guessed route.
#[derive(Clone, Debug)]
pub struct DraftRestore {
    pub draft: StoredDraft,
    pub expected_version: u32,
    pub submitted: bool,
    pub payee_query: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftView {
    /// The foundation has no account-list method. Never fabricate candidates.
    NeedAccountSource,
    NeedPayeeSearch,
    NeedPayee(Vec<PayeeView>),
    NeedAmount,
    InvalidAmount,
    ReadyForReview,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftProgress {
    pub task_id: TaskId,
    pub view: DraftView,
    pub saved: Option<StoredDraft>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrationError {
    Bank(BankError),
    Draft(DraftError),
    TaskMismatch,
    MappingError,
    AuthorizationRejected,
    DraftAlreadySubmitted,
    InvalidRemark,
}
impl From<BankError> for IntegrationError {
    fn from(error: BankError) -> Self {
        Self::Bank(error)
    }
}
impl From<DraftError> for IntegrationError {
    fn from(error: DraftError) -> Self {
        Self::Draft(error)
    }
}
impl std::fmt::Display for IntegrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for IntegrationError {}

#[derive(Clone)]
struct SelectionState {
    user: UserId,
    account: Option<ResourceId>,
    query: Option<String>,
    payee: Option<PayeeSelection>,
    draft_id: Option<ResourceId>,
}
impl SelectionState {
    fn apply(&mut self, input: &DraftInput) {
        if let Some(account) = &input.payer_account_id {
            self.account = Some(account.clone());
        }
        if let Some(query) = &input.payee_query {
            self.query = Some(query.clone());
        }
        if let Some(payee) = &input.payee {
            self.payee = Some(payee.clone());
            if let PayeeSelection::Name(name) = payee {
                if input.payee_query.is_none() {
                    self.query = Some(name.clone());
                }
            }
        }
    }
}

#[derive(Clone)]
struct BankReferences {
    account: Option<GetAccountBalanceOutput>,
    payees: FindPayeesOutput,
}

// Short-lived adapter for the unchanged synchronous draft module. It has no
// bank fixture/default user; data is installed only after formal async reads.
#[derive(Clone, Default)]
struct Snapshot(Arc<Mutex<Option<(String, legacy::BankCatalog)>>>);
impl legacy::BankGateway for Snapshot {
    fn catalog(&self, user: &str) -> Result<legacy::BankCatalog, legacy::GatewayError> {
        self.0
            .lock()
            .map_err(|_| legacy::GatewayError::Unavailable)?
            .as_ref()
            .filter(|(owner, _)| owner == user)
            .map(|(_, catalog)| catalog.clone())
            .ok_or(legacy::GatewayError::Unavailable)
    }
}

/// Server-only, serial in-memory module. A context MUST come from the trusted
/// foundation; constructing a typed context is not authentication. Not a Tool.
pub struct TransferDrafts<C> {
    bank: Arc<dyn BankGateway>,
    snapshot: Snapshot,
    drafts: legacy::DraftService<Snapshot, C>,
    selections: HashMap<TaskId, SelectionState>,
    references: HashMap<TaskId, BankReferences>,
}

impl<C: Clock> TransferDrafts<C> {
    pub fn new(
        bank: Arc<dyn BankGateway>,
        clock: C,
        ttl_seconds: u64,
    ) -> Result<Self, IntegrationError> {
        let snapshot = Snapshot::default();
        Ok(Self {
            bank,
            drafts: legacy::DraftService::new(snapshot.clone(), clock, ttl_seconds)?,
            snapshot,
            selections: HashMap::new(),
            references: HashMap::new(),
        })
    }

    /// Rehydrates business state only. It does not read SQL, authenticate the
    /// caller, authorize editing, refresh bank facts, or restore old permits.
    /// Modify still performs fresh bank reads; cancel can work bank-offline.
    pub fn restore_for_backend(
        &mut self,
        context: &ExecutionContext,
        recovery: DraftRestore,
    ) -> Result<StoredDraft, IntegrationError> {
        let saved = &recovery.draft;
        if saved.user_id != context.authenticated_user_id {
            return Err(DraftError::AccessDenied.into());
        }
        if saved.task_id != context.task_id {
            return Err(IntegrationError::TaskMismatch);
        }
        if recovery.submitted {
            return Err(IntegrationError::DraftAlreadySubmitted);
        }
        check_active_version(saved, recovery.expected_version)?;
        if saved.plan_version == 0 {
            return Err(DraftError::InvalidRestore.into());
        }
        if self.selections.contains_key(&saved.task_id) {
            return Err(DraftError::AlreadyCreated.into());
        }
        for id in [
            saved.draft_id.as_str(),
            saved.operation_id.as_str(),
            saved.task_id.as_str(),
            saved.user_id.as_str(),
        ] {
            GetAccountBalanceInput {
                account_id: ResourceId::parse(id).map_err(|_| IntegrationError::MappingError)?,
            }
            .validate()?;
        }
        FindPayeesInput {
            query: recovery.payee_query.clone(),
        }
        .validate()?;
        let data = saved.details.as_ref().ok_or(DraftError::NotReady)?;
        let account = GetAccountBalanceOutput {
            account: data.account.clone(),
            available_balance: data.available_balance.clone(),
            as_of: data.balance_as_of.clone(),
        };
        account.validate_for(&data.account.account_id)?;
        let payees = FindPayeesOutput {
            candidates: vec![data.payee.clone()],
        };
        payees.validate()?;
        // Reuse the existing exact amount/remark validation; no funds are issued.
        mock_execution::MockTransferRequest::from_trusted_draft(
            context,
            saved,
            recovery.expected_version,
            0,
        )?;
        let legacy = legacy::StoredDraft {
            draft_id: saved.draft_id.to_string(),
            operation_id: saved.operation_id.to_string(),
            task_id: saved.task_id.to_string(),
            user_id: saved.user_id.to_string(),
            plan_version: saved.plan_version.into(),
            status: saved.status,
            expires_at: saved.expires_at,
            details: Some(legacy::DraftDetails {
                account: legacy::Account {
                    id: data.account.account_id.to_string(),
                    label: data.account.display_name.clone(),
                    last_four: data.account.masked_account_no[5..].to_owned(),
                },
                payee: legacy::Payee {
                    id: data.payee.payee_id.to_string(),
                    name: data.payee.display_name.clone(),
                    last_four: data.payee.masked_account_no[5..].to_owned(),
                },
                amount: data.amount.as_str().into(),
                currency: "CNY".into(),
                remark: data.remark.clone(),
            }),
        };
        self.drafts
            .restore_for_backend(context.authenticated_user_id.as_str(), legacy)?;
        self.selections.insert(
            saved.task_id.clone(),
            SelectionState {
                user: saved.user_id.clone(),
                account: Some(data.account.account_id.clone()),
                query: Some(recovery.payee_query),
                payee: Some(PayeeSelection::Id(data.payee.payee_id.clone())),
                draft_id: Some(saved.draft_id.clone()),
            },
        );
        self.references.insert(
            saved.task_id.clone(),
            BankReferences {
                account: Some(account),
                payees,
            },
        );
        Ok(recovery.draft)
    }

    pub async fn create(
        &mut self,
        context: &ExecutionContext,
        input: DraftInput,
        remark: &str,
    ) -> Result<DraftProgress, IntegrationError> {
        self.create_guarded(context, input, remark, || Ok(())).await
    }

    /// The guard runs after async bank reads and before the existing synchronous
    /// business save, so an expired/revoked authorization cannot save a draft.
    pub async fn create_guarded<G>(
        &mut self,
        context: &ExecutionContext,
        input: DraftInput,
        remark: &str,
        before_save: impl FnOnce() -> Result<G, IntegrationError>,
    ) -> Result<DraftProgress, IntegrationError> {
        let mut selection = self
            .selections
            .get(&context.task_id)
            .cloned()
            .unwrap_or_else(|| SelectionState {
                user: context.authenticated_user_id.clone(),
                account: None,
                query: None,
                payee: None,
                draft_id: None,
            });
        if selection.user != context.authenticated_user_id {
            return Err(DraftError::AccessDenied.into());
        }
        if selection.draft_id.is_some() {
            return Err(DraftError::AlreadyCreated.into());
        }
        validate_draft_remark(remark)?;
        selection.apply(&input);
        let references = self.fetch(&selection).await?;
        let _authorization = before_save()?;
        self.install(&selection.user, &references)?;
        let progress = self.drafts.create(
            selection.user.as_str(),
            Some(context.task_id.as_str()),
            legacy_input(input),
            remark,
        );
        self.clear_snapshot()?;
        let progress = progress?;
        let result = Self::progress(progress, &references, &selection)?;
        selection.draft_id = result.saved.as_ref().map(|draft| draft.draft_id.clone());
        self.references.insert(context.task_id.clone(), references);
        self.selections.insert(context.task_id.clone(), selection);
        Ok(result)
    }

    /// Historical snapshot: no fresh bank read, no permission to transfer.
    pub fn read(
        &mut self,
        context: &ExecutionContext,
        draft_id: &ResourceId,
    ) -> Result<StoredDraft, IntegrationError> {
        let saved = self
            .drafts
            .read(context.authenticated_user_id.as_str(), draft_id.as_str())?;
        if saved.task_id != context.task_id.as_str() {
            return Err(IntegrationError::TaskMismatch);
        }
        let references = self
            .references
            .get(&context.task_id)
            .ok_or(IntegrationError::MappingError)?;
        Self::stored(saved, references)
    }

    /// Re-checks the saved version/lifecycle only. Not confirmation, not a permit,
    /// and not a fresh balance/ownership guarantee for a later submission.
    pub fn current_for_review(
        &mut self,
        context: &ExecutionContext,
        draft_id: &ResourceId,
        expected_version: u32,
    ) -> Result<StoredDraft, IntegrationError> {
        self.read(context, draft_id)?;
        let saved = self.drafts.current_for_review(
            context.authenticated_user_id.as_str(),
            draft_id.as_str(),
            expected_version.into(),
        )?;
        Self::stored(saved, &self.references[&context.task_id])
    }

    pub async fn modify(
        &mut self,
        context: &ExecutionContext,
        draft_id: &ResourceId,
        expected_version: u32,
        changes: DraftChanges,
    ) -> Result<DraftProgress, IntegrationError> {
        self.modify_inner(context, draft_id, expected_version, changes, false, || {
            Ok(())
        })
        .await
    }

    /// Runtime's complete-draft path: reject missing fresh bank references before
    /// mutation, then hold the supplied guard through the synchronous save.
    pub async fn modify_guarded<G>(
        &mut self,
        context: &ExecutionContext,
        draft_id: &ResourceId,
        expected_version: u32,
        changes: DraftChanges,
        before_save: impl FnOnce() -> Result<G, IntegrationError>,
    ) -> Result<DraftProgress, IntegrationError> {
        self.modify_inner(
            context,
            draft_id,
            expected_version,
            changes,
            true,
            before_save,
        )
        .await
    }

    async fn modify_inner<G>(
        &mut self,
        context: &ExecutionContext,
        draft_id: &ResourceId,
        expected_version: u32,
        changes: DraftChanges,
        require_complete: bool,
        before_save: impl FnOnce() -> Result<G, IntegrationError>,
    ) -> Result<DraftProgress, IntegrationError> {
        let saved = self.read(context, draft_id)?;
        check_active_version(&saved, expected_version)?;
        if let Some(remark) = changes.remark.as_deref() {
            validate_draft_remark(remark)?;
        }
        if changes
            .currency
            .as_deref()
            .is_some_and(|currency| currency != "CNY")
        {
            return Err(DraftError::UnsupportedCurrency.into());
        }
        let has_changes = changes.input.payer_account_id.is_some()
            || changes.input.payee_query.is_some()
            || changes.input.payee.is_some()
            || changes.input.amount.is_some()
            || changes.currency.is_some()
            || changes.remark.is_some();
        if has_changes && saved.plan_version == u32::MAX {
            return Err(DraftError::VersionOverflow.into());
        }
        let mut selection = self.selections[&context.task_id].clone();
        selection.apply(&changes.input);
        let references = if has_changes {
            self.fetch(&selection).await?
        } else {
            self.references[&context.task_id].clone()
        };
        if require_complete {
            let selected_payee = match &selection.payee {
                Some(PayeeSelection::Id(id)) => id,
                _ => return Err(DraftError::NotReady.into()),
            };
            if references.account.is_none()
                || !references
                    .payees
                    .candidates
                    .iter()
                    .any(|p| &p.payee_id == selected_payee)
            {
                return Err(DraftError::NotReady.into());
            }
        }
        let _authorization = before_save()?;
        self.install(&selection.user, &references)?;
        // Search edits are parameter changes too, even if no explicit selection
        // was supplied. The legacy currency field forces exactly one increment.
        let currency = if changes.input.payee_query.is_some() {
            Some("CNY".into())
        } else {
            changes.currency
        };
        let progress = self.drafts.modify(
            selection.user.as_str(),
            draft_id.as_str(),
            expected_version.into(),
            legacy::DraftChanges {
                input: legacy_input(changes.input),
                currency,
                remark: changes.remark,
            },
        );
        self.clear_snapshot()?;
        let result = Self::progress(progress?, &references, &selection)?;
        self.references.insert(context.task_id.clone(), references);
        self.selections.insert(context.task_id.clone(), selection);
        Ok(result)
    }

    /// Only unsubmitted in-memory drafts exist here; never cancels a bank order.
    pub fn cancel(
        &mut self,
        context: &ExecutionContext,
        draft_id: &ResourceId,
        expected_version: u32,
    ) -> Result<StoredDraft, IntegrationError> {
        self.cancel_guarded(context, draft_id, expected_version, || Ok(()))
    }

    pub fn cancel_guarded<G>(
        &mut self,
        context: &ExecutionContext,
        draft_id: &ResourceId,
        expected_version: u32,
        before_save: impl FnOnce() -> Result<G, IntegrationError>,
    ) -> Result<StoredDraft, IntegrationError> {
        self.read(context, draft_id)?;
        let _authorization = before_save()?;
        let saved = self.drafts.cancel(
            context.authenticated_user_id.as_str(),
            draft_id.as_str(),
            expected_version.into(),
        )?;
        Self::stored(saved, &self.references[&context.task_id])
    }

    async fn fetch(&self, selection: &SelectionState) -> Result<BankReferences, IntegrationError> {
        if let Some(PayeeSelection::Id(id)) = &selection.payee {
            // The existing input validator provides the same opaque ResourceId
            // constraints; this validation does not issue an account query.
            GetAccountBalanceInput {
                account_id: id.clone(),
            }
            .validate()?;
        }
        let account = if let Some(id) = &selection.account {
            GetAccountBalanceInput {
                account_id: id.clone(),
            }
            .validate()?;
            let output = self.bank.get_account_balance(&selection.user, id).await?;
            output.validate_for(id)?;
            Some(output)
        } else {
            None
        };
        let payees = if let Some(query) = &selection.query {
            FindPayeesInput {
                query: query.clone(),
            }
            .validate()?;
            let output = self.bank.find_payees(&selection.user, query).await?;
            output.validate()?;
            output
        } else {
            FindPayeesOutput { candidates: vec![] }
        };
        Ok(BankReferences { account, payees })
    }

    fn install(&self, user: &UserId, references: &BankReferences) -> Result<(), IntegrationError> {
        let catalog = legacy::BankCatalog {
            accounts: references
                .account
                .iter()
                .map(|output| legacy::BankAccount {
                    account: legacy::Account {
                        id: output.account.account_id.to_string(),
                        label: output.account.display_name.clone(),
                        last_four: output.account.masked_account_no[5..].to_owned(),
                    },
                    balance: output.available_balance.as_str().into(),
                    currency: "CNY".into(),
                })
                .collect(),
            payees: references
                .payees
                .candidates
                .iter()
                .map(|payee| legacy::Payee {
                    id: payee.payee_id.to_string(),
                    name: payee.display_name.clone(),
                    last_four: payee.masked_account_no[5..].to_owned(),
                })
                .collect(),
        };
        *self
            .snapshot
            .0
            .lock()
            .map_err(|_| IntegrationError::MappingError)? = Some((user.to_string(), catalog));
        Ok(())
    }

    fn clear_snapshot(&self) -> Result<(), IntegrationError> {
        self.snapshot
            .0
            .lock()
            .map_err(|_| IntegrationError::MappingError)?
            .take();
        Ok(())
    }

    fn progress(
        progress: legacy::DraftProgress,
        references: &BankReferences,
        selection: &SelectionState,
    ) -> Result<DraftProgress, IntegrationError> {
        let view = match &progress.view {
            legacy::DraftView::NeedAccount(_) => DraftView::NeedAccountSource,
            legacy::DraftView::NeedPayee(_) if selection.query.is_none() => {
                DraftView::NeedPayeeSearch
            }
            legacy::DraftView::NeedPayee(choices) => DraftView::NeedPayee(
                references
                    .payees
                    .candidates
                    .iter()
                    .filter(|payee| {
                        choices
                            .iter()
                            .any(|choice| choice.id == payee.payee_id.as_str())
                    })
                    .cloned()
                    .collect(),
            ),
            legacy::DraftView::NeedAmount => DraftView::NeedAmount,
            legacy::DraftView::InvalidAmount => DraftView::InvalidAmount,
            legacy::DraftView::AwaitingConfirmation(_) => DraftView::ReadyForReview,
        };
        Ok(DraftProgress {
            task_id: TaskId::parse(progress.task_id).map_err(|_| IntegrationError::MappingError)?,
            view,
            saved: progress
                .saved
                .map(|draft| Self::stored(draft, references))
                .transpose()?,
        })
    }

    fn stored(
        draft: legacy::StoredDraft,
        references: &BankReferences,
    ) -> Result<StoredDraft, IntegrationError> {
        let details = draft
            .details
            .map(|details| {
                let account = references
                    .account
                    .as_ref()
                    .filter(|output| output.account.account_id.as_str() == details.account.id)
                    .ok_or(IntegrationError::MappingError)?;
                let payee = references
                    .payees
                    .candidates
                    .iter()
                    .find(|payee| payee.payee_id.as_str() == details.payee.id)
                    .ok_or(IntegrationError::MappingError)?;
                Ok::<_, IntegrationError>(DraftDetails {
                    account: account.account.clone(),
                    payee: payee.clone(),
                    amount: DecimalAmount::parse(details.amount)?,
                    currency: CnyCurrency::Cny,
                    remark: details.remark,
                    available_balance: account.available_balance.clone(),
                    balance_as_of: account.as_of.clone(),
                })
            })
            .transpose()?;
        Ok(StoredDraft {
            draft_id: ResourceId::parse(draft.draft_id)
                .map_err(|_| IntegrationError::MappingError)?,
            task_id: TaskId::parse(draft.task_id).map_err(|_| IntegrationError::MappingError)?,
            operation_id: OperationId::parse(draft.operation_id)
                .map_err(|_| IntegrationError::MappingError)?,
            user_id: UserId::parse(draft.user_id).map_err(|_| IntegrationError::MappingError)?,
            plan_version: draft
                .plan_version
                .try_into()
                .map_err(|_| DraftError::VersionOverflow)?,
            status: draft.status,
            expires_at: draft.expires_at,
            details,
        })
    }
}

// REMARK-140-v1 also applies to direct backend callers, not just Tool parsing.
fn validate_draft_remark(remark: &str) -> Result<(), IntegrationError> {
    if remark.chars().count() > 140 || remark.chars().any(char::is_control) {
        return Err(IntegrationError::InvalidRemark);
    }
    Ok(())
}

fn legacy_input(input: DraftInput) -> legacy::DraftInput {
    legacy::DraftInput {
        account: input
            .payer_account_id
            .map(|id| legacy::Selection::Id(id.to_string())),
        payee: input.payee.map(|selection| match selection {
            PayeeSelection::Name(name) => legacy::Selection::Name(name),
            PayeeSelection::Id(id) => legacy::Selection::Id(id.to_string()),
        }),
        amount_yuan: input.amount,
    }
}

fn check_active_version(saved: &StoredDraft, expected: u32) -> Result<(), IntegrationError> {
    if expected != saved.plan_version {
        return Err(DraftError::VersionConflict {
            expected: expected.into(),
            actual: saved.plan_version.into(),
        }
        .into());
    }
    match saved.status {
        DraftStatus::Expired => Err(DraftError::Expired.into()),
        DraftStatus::Cancelled => Err(DraftError::Cancelled.into()),
        _ => Ok(()),
    }
}
