//! Transfer-owned SQL business implementation, NOT a public authorization entry.
//! Every write requires the caller's already-open transaction connection. The
//! foundation owns permit consumption, final safety checks, audit/outbox and
//! commit/rollback; propagate EVERY error and roll back the ENTIRE transaction.
//! Never call from an LLM/client or with an autocommit connection.
use crate::business_store::{BusinessRecord, BusinessStore, StoreError};
use crate::mock_execution::{
    validate_request, MockAccountFunds, MockBehavior, MockOutgoingState, MockResolution,
    MockTransferReceipt, MockTransferRequest, MockTransferTransaction, TransferDirection,
};
use crate::{DraftDetails, DraftStatus, StoredDraft};
use safeflow_bank_gateway::{
    CnyCurrency, DecimalAmount, FindPayeesOutput, GetAccountBalanceOutput,
};
use safeflow_contracts::{
    IdempotencyKey, OperationId, ResourceId, TaskId, ToolResultStatus, UserId,
};
use sqlx::{postgres::PgRow, PgConnection, Row};

pub const PROPOSED_SCHEMA: &str = include_str!("../sql/funds_v1.proposed.sql");

#[derive(Debug)]
pub enum FundsError {
    Invalid,
    NotFound,
    VersionConflict,
    DraftInactive,
    AlreadySubmitted,
    Conflict,
    OutgoingUnknown,
    TransactionRequired,
    Receipt(StoreError),
    Database(sqlx::Error),
}
impl std::fmt::Display for FundsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "INVALID_TRANSFER_FACT",
            Self::NotFound => "TRANSFER_NOT_FOUND",
            Self::VersionConflict => "DRAFT_VERSION_CONFLICT",
            Self::DraftInactive => "DRAFT_INACTIVE",
            Self::AlreadySubmitted => "DRAFT_ALREADY_SUBMITTED",
            Self::Conflict => "TRANSFER_FACT_CONFLICT",
            Self::OutgoingUnknown => "OUTGOING_STATE_UNKNOWN",
            Self::TransactionRequired => "CALLER_READ_COMMITTED_TRANSACTION_REQUIRED",
            Self::Receipt(_) => "TRANSFER_RECEIPT_CONFLICT_OR_UNAVAILABLE",
            Self::Database(_) => "TRANSFER_STORAGE_UNAVAILABLE",
        })
    }
}
impl std::error::Error for FundsError {}
impl From<sqlx::Error> for FundsError {
    fn from(value: sqlx::Error) -> Self {
        Self::Database(value)
    }
}
impl From<StoreError> for FundsError {
    fn from(value: StoreError) -> Self {
        Self::Receipt(value)
    }
}

/// Supplemental, backend-only accounting evidence. Not PostExecuteAssessment,
/// confirmation, a retry instruction, or proof of public Runtime integration.
/// Recipient balances/identity are deliberately absent from the returned value.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AccountingCheck {
    pub record: BusinessRecord,
    pub payer_funds: MockAccountFunds,
    pub operation_reserved: DecimalAmount,
}

/// Trusted backend reader invoked AFTER the existing business/account locks.
/// It must read only the supplied owner's payer account on this SAME connection,
/// propagate unavailable/incomplete facts, and never commit, roll back, open a
/// second connection, change isolation, or release locks. All card writers must
/// honor the same account protection through the caller's final COMMIT.
///
/// This is a transfer-owned integration seam, not a public authorization contract.
/// Allowed means only that this source does not block outgoing funds; the
/// foundation still owns identity, confirmation, permit and other safety checks.
/// A PgConnection borrow is NOT a sandbox against an untrusted implementation.
pub trait LockedOutgoingSource: Send {
    fn read<'a>(
        &'a mut self,
        connection: &'a mut PgConnection,
        owner: &'a UserId,
        payer: &'a ResourceId,
    ) -> futures::future::BoxFuture<'a, Result<MockOutgoingState, FundsError>>;
}

enum OutgoingCheck<'a> {
    Fixed(MockOutgoingState),
    Current(&'a mut dyn LockedOutgoingSource),
}
impl OutgoingCheck<'_> {
    async fn ensure_transaction(&mut self, tx: &mut PgConnection) -> Result<(), FundsError> {
        if matches!(self, Self::Current(_)) {
            outgoing_transaction(tx).await?;
        }
        Ok(())
    }

    async fn read(
        self,
        tx: &mut PgConnection,
        owner: &UserId,
        payer: &ResourceId,
    ) -> Result<MockOutgoingState, FundsError> {
        match self {
            Self::Fixed(state) => Ok(state),
            Self::Current(source) => {
                let before = outgoing_transaction(tx).await?;
                let result = source.read(tx, owner, payer).await?;
                if outgoing_transaction(tx).await? != before {
                    // Detect a broken reader contract; this cannot undo anything
                    // an untrusted reader already committed on its own.
                    return Err(FundsError::TransactionRequired);
                }
                Ok(result)
            }
        }
    }
}

async fn outgoing_transaction(tx: &mut PgConnection) -> Result<String, FundsError> {
    let (id, isolation): (Option<String>, String) = sqlx::query_as(
        "SELECT txid_current_if_assigned()::text,current_setting('transaction_isolation')",
    )
    .fetch_one(tx)
    .await?;
    if isolation != "read committed" {
        return Err(FundsError::TransactionRequired);
    }
    id.ok_or(FundsError::TransactionRequired)
}

pub struct FundsStore;
impl FundsStore {
    /// Read-only corroboration on the caller's connection. Does not replace the
    /// foundation's original-key/request/paired-ledger verification or advance
    /// public states. Missing original means unavailable, NOT proven unsubmitted.
    pub async fn check_accounting(
        connection: &mut PgConnection,
        owner: &UserId,
        operation: &OperationId,
    ) -> Result<Option<AccountingCheck>, FundsError> {
        valid_id(owner.as_str())?;
        valid_id(operation.as_str())?;
        let Some(record) = Self::query(connection, owner, operation).await? else {
            return Ok(None);
        };
        // One statement observes both account balances, all their ledger entries
        // and holds together. Compare the original again rather than mixing a
        // receipt observed before settlement with balances observed afterwards.
        // The foundation still owns transaction isolation/protection through commit.
        let rows = sqlx::query(
            "SELECT b.original_key,b.request,b.first_receipt,b.latest_receipt,
                f.payer_account_id,f.amount_cents,f.state,a.account_id,a.owner_id,
                a.booked_cents,
                a.booked_cents::numeric = a.opening_cents::numeric + simulation_balance_net_v1(a.account_id) +
                  COALESCE((SELECT SUM(CASE WHEN l.direction='CREDIT'
                    THEN l.amount_cents::numeric ELSE -l.amount_cents::numeric END)
                    FROM transfer_ledger_v1 l WHERE l.account_id=a.account_id),0) AS balanced,
                COALESCE((SELECT SUM(h.amount_cents::numeric)
                    FROM transfer_fund_orders_v1 h
                    WHERE h.payer_account_id=a.account_id AND h.state='PROCESSING'),0)::text AS reserved
             FROM transfer_business_orders_v1 b
             JOIN transfer_fund_orders_v1 f USING(owner_id,operation_id)
             JOIN transfer_accounts_v1 a ON a.account_id IN(f.payer_account_id,f.recipient_account_id)
             WHERE b.owner_id=$1 AND b.operation_id=$2",
        ).bind(owner.as_str()).bind(operation.as_str()).fetch_all(connection).await?;
        if rows.len() != 2 {
            return Err(FundsError::Conflict);
        }
        let mut payer_funds = None;
        for row in rows {
            let current = crate::business_store::decode(&row, owner)?;
            let state: &str = row.try_get("state")?;
            let expected_state = match record.latest_receipt.status {
                ToolResultStatus::Processing => "PROCESSING",
                ToolResultStatus::Succeeded => "SUCCEEDED",
                ToolResultStatus::Failed => "FAILED",
            };
            if current != record
                || row.try_get::<&str, _>("payer_account_id")?
                    != record.request.payer_account_id.as_str()
                || row.try_get::<i64, _>("amount_cents")? != cents(&record.request.amount)?
                || state != expected_state
                || !row.try_get::<bool, _>("balanced")?
            {
                return Err(FundsError::Conflict);
            }
            let booked: i64 = row.try_get("booked_cents")?;
            let reserved: i64 = row
                .try_get::<&str, _>("reserved")?
                .parse()
                .map_err(|_| FundsError::Conflict)?;
            if reserved < 0 || reserved > booked {
                return Err(FundsError::Conflict);
            }
            if row.try_get::<&str, _>("account_id")? == record.request.payer_account_id.as_str() {
                if row.try_get::<&str, _>("owner_id")? != owner.as_str() {
                    return Err(FundsError::Conflict);
                }
                payer_funds = Some(MockAccountFunds {
                    booked: decimal(booked)?,
                    reserved: decimal(reserved)?,
                    available: decimal(booked - reserved)?,
                });
            }
        }
        let payer_funds = payer_funds.ok_or(FundsError::Conflict)?;
        let operation_reserved = if record.latest_receipt.status == ToolResultStatus::Processing {
            record.request.amount.clone()
        } else {
            decimal(0)?
        };
        Ok(Some(AccountingCheck {
            record,
            payer_funds,
            operation_reserved,
        }))
    }

    /// Read-only business facts on the FOUNDATION'S connection. The caller must
    /// provide a consistent snapshot and recheck/protect mutable facts through
    /// COMMIT. Multiple reads here do not create a transaction or a permit.
    /// Historical orders are read first; old drafts/balances are not prerequisites
    /// to query an existing operation. Database errors never become empty facts.
    pub async fn policy_facts(
        connection: &mut PgConnection,
        owner: &UserId,
        key: &IdempotencyKey,
        request: &MockTransferRequest,
    ) -> Result<crate::funds_policy::FundsStoredFacts, FundsError> {
        validate_request(owner, key, request).map_err(|_| FundsError::Invalid)?;
        let original = Self::query(connection, owner, &request.operation_id).await?;
        let original_key: Option<String> = sqlx::query_scalar("SELECT original_key FROM transfer_business_orders_v1 WHERE owner_id=$1 AND operation_id=$2")
            .bind(owner.as_str()).bind(request.operation_id.as_str()).fetch_optional(&mut *connection).await?;
        let key_operation: Option<String> = sqlx::query_scalar("SELECT operation_id FROM transfer_business_orders_v1 WHERE owner_id=$1 AND original_key=$2")
            .bind(owner.as_str()).bind(key.as_str()).fetch_optional(&mut *connection).await?;
        let mut facts = crate::funds_policy::FundsStoredFacts {
            owner_id: owner.clone(),
            request: request.clone(),
            key: key.clone(),
            draft: None,
            submitted: false,
            payer_owner_id: None,
            funds: None,
            recipient_account_id: None,
            original_request: original.map(|record| record.request),
            original_key: original_key
                .map(IdempotencyKey::parse)
                .transpose()
                .map_err(|_| FundsError::Invalid)?,
            key_operation_id: key_operation
                .map(OperationId::parse)
                .transpose()
                .map_err(|_| FundsError::Invalid)?,
        };
        if facts.original_request.is_some()
            || facts.original_key.is_some()
            || facts.key_operation_id.is_some()
        {
            return Ok(facts);
        }
        if let Some(row) =
            sqlx::query("SELECT * FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$2")
                .bind(owner.as_str())
                .bind(request.draft_id.as_str())
                .fetch_optional(&mut *connection)
                .await?
        {
            facts.submitted = row.try_get("submitted")?;
            facts.draft = Some(decode_draft(&row)?);
        }
        // Do not read another user's balance or disclose their owner identifier.
        let owned: Option<String> = sqlx::query_scalar(
            "SELECT owner_id FROM transfer_accounts_v1 WHERE owner_id=$1 AND account_id=$2",
        )
        .bind(owner.as_str())
        .bind(request.payer_account_id.as_str())
        .fetch_optional(&mut *connection)
        .await?;
        facts.payer_owner_id = owned
            .map(UserId::parse)
            .transpose()
            .map_err(|_| FundsError::Invalid)?;
        if facts.payer_owner_id.is_some() {
            facts.funds = Some(Self::funds(connection, owner, &request.payer_account_id).await?);
        }
        let recipient: Option<String> = sqlx::query_scalar("SELECT recipient_account_id FROM transfer_payee_routes_v1 WHERE owner_id=$1 AND payee_id=$2")
            .bind(owner.as_str()).bind(request.payee_id.as_str()).fetch_optional(connection).await?;
        facts.recipient_account_id = recipient
            .map(ResourceId::parse)
            .transpose()
            .map_err(|_| FundsError::Invalid)?;
        Ok(facts)
    }

    /// Trusted provisioning ONLY, not an API or identity source. Replay never
    /// resets the balance. These proposed tables need foundation migration review.
    pub async fn provision_account(
        tx: &mut PgConnection,
        owner: &UserId,
        account: &ResourceId,
        opening: &DecimalAmount,
    ) -> Result<(), FundsError> {
        valid_id(owner.as_str())?;
        valid_id(account.as_str())?;
        let opening = cents(opening)?;
        sqlx::query("INSERT INTO transfer_accounts_v1(account_id,owner_id,opening_cents,booked_cents) VALUES($1,$2,$3,$3) ON CONFLICT DO NOTHING")
            .bind(account.as_str()).bind(owner.as_str()).bind(opening).execute(&mut *tx).await?;
        let row=sqlx::query("SELECT owner_id,opening_cents FROM transfer_accounts_v1 WHERE account_id=$1 FOR UPDATE").bind(account.as_str()).fetch_one(tx).await?;
        if row.try_get::<&str, _>("owner_id")? != owner.as_str()
            || row.try_get::<i64, _>("opening_cents")? != opening
        {
            return Err(FundsError::Conflict);
        }
        Ok(())
    }

    /// Stable trusted routing, NEVER derived from a name or a masked tail.
    pub async fn provision_payee(
        tx: &mut PgConnection,
        owner: &UserId,
        payee: &ResourceId,
        recipient: &ResourceId,
    ) -> Result<(), FundsError> {
        for id in [owner.as_str(), payee.as_str(), recipient.as_str()] {
            valid_id(id)?;
        }
        sqlx::query("INSERT INTO transfer_payee_routes_v1(owner_id,payee_id,recipient_account_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(owner.as_str()).bind(payee.as_str()).bind(recipient.as_str()).execute(&mut *tx).await?;
        let actual:String=sqlx::query_scalar("SELECT recipient_account_id FROM transfer_payee_routes_v1 WHERE owner_id=$1 AND payee_id=$2 FOR UPDATE")
            .bind(owner.as_str()).bind(payee.as_str()).fetch_one(tx).await?;
        if actual != recipient.as_str() {
            return Err(FundsError::Conflict);
        }
        Ok(())
    }

    /// SAME CONNECTION only. behavior is a trusted simulated-bank fixture knob,
    /// not a customer-selected result. outgoing must be a fresh authoritative
    /// account decision protected through COMMIT by foundation (not old UI data).
    /// This argument is deliberately NOT a permit and NOT serde-deserializable.
    /// Replay returns the historical record without authorizing another payment.
    /// Legacy fixture/protected-fact entry. New card-backed adapters should use
    /// submit_checked so the reader runs inside the existing account protection.
    pub async fn submit(
        tx: &mut PgConnection,
        owner: &UserId,
        key: &IdempotencyKey,
        request: &MockTransferRequest,
        behavior: MockBehavior,
        outgoing: MockOutgoingState,
    ) -> Result<BusinessRecord, FundsError> {
        Self::submit_inner(
            tx,
            owner,
            key,
            request,
            behavior,
            OutgoingCheck::Fixed(outgoing),
        )
        .await
    }

    /// Same business behavior as submit, with a fresh backend source called after
    /// request/draft/route/account protection. The source is skipped on replay.
    /// Requires the caller's READ COMMITTED transaction; does not create a permit
    /// or adopt the foundation's complete outer lock ordering.
    pub async fn submit_checked(
        tx: &mut PgConnection,
        owner: &UserId,
        key: &IdempotencyKey,
        request: &MockTransferRequest,
        behavior: MockBehavior,
        source: &mut dyn LockedOutgoingSource,
    ) -> Result<BusinessRecord, FundsError> {
        Self::submit_inner(
            tx,
            owner,
            key,
            request,
            behavior,
            OutgoingCheck::Current(source),
        )
        .await
    }

    async fn submit_inner(
        tx: &mut PgConnection,
        owner: &UserId,
        key: &IdempotencyKey,
        request: &MockTransferRequest,
        behavior: MockBehavior,
        outgoing: OutgoingCheck<'_>,
    ) -> Result<BusinessRecord, FundsError> {
        validate_request(owner, key, request).map_err(|_| FundsError::Invalid)?;
        let amount = cents(&request.amount)?;
        if amount == 0 || request.draft_version == 0 {
            return Err(FundsError::Invalid);
        }
        request_locks(tx, owner, key, &request.operation_id).await?;
        if let Some(operation)=sqlx::query_scalar::<_,String>("SELECT operation_id FROM transfer_business_orders_v1 WHERE owner_id=$1 AND original_key=$2")
            .bind(owner.as_str()).bind(key.as_str()).fetch_optional(&mut *tx).await? {
            if operation!=request.operation_id.as_str() {return Err(StoreError::IdempotencyConflict.into());}
            let record=Self::query(tx,owner,&request.operation_id).await?.ok_or(FundsError::Conflict)?;
            if record.request!=*request {return Err(StoreError::IdempotencyConflict.into());}
            return Ok(record);
        }
        if BusinessStore::query(tx, owner, &request.operation_id)
            .await?
            .is_some()
        {
            return Err(StoreError::OperationConflict.into());
        }
        let row = sqlx::query(
            "SELECT * FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$2 FOR UPDATE",
        )
        .bind(owner.as_str())
        .bind(request.draft_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(FundsError::NotFound)?;
        let draft = decode_draft(&row)?;
        if row.try_get::<bool, _>("submitted")? {
            return Err(FundsError::AlreadySubmitted);
        }
        check_request(&draft, request)?;
        let recipient:String=sqlx::query_scalar("SELECT recipient_account_id FROM transfer_payee_routes_v1 WHERE owner_id=$1 AND payee_id=$2 FOR SHARE")
            .bind(owner.as_str()).bind(request.payee_id.as_str()).fetch_optional(&mut *tx).await?.ok_or(FundsError::NotFound)?;
        let recipient = ResourceId::parse(recipient).map_err(|_| FundsError::Invalid)?;
        if recipient == request.payer_account_id {
            return Err(FundsError::Invalid);
        }
        lock_accounts(tx, &request.payer_account_id, &recipient).await?;
        let funds = Self::funds(tx, owner, &request.payer_account_id).await?;
        if now(tx).await? >= draft.expires_at || draft.status != DraftStatus::ReadyToConfirm {
            return Err(FundsError::DraftInactive);
        }
        let outgoing = outgoing.read(tx, owner, &request.payer_account_id).await?;
        // A slow authoritative read must not carry a draft across its expiry.
        if now(tx).await? >= draft.expires_at {
            return Err(FundsError::DraftInactive);
        }
        let reason = match outgoing {
            MockOutgoingState::Unknown => return Err(FundsError::OutgoingUnknown),
            MockOutgoingState::Blocked => Some("ACCOUNT_OUTGOING_BLOCKED"),
            MockOutgoingState::Allowed if cents(&funds.available)? < amount => {
                Some("INSUFFICIENT_FUNDS")
            }
            MockOutgoingState::Allowed if matches!(behavior, MockBehavior::Decline) => {
                Some("MOCK_BANK_DECLINED")
            }
            MockOutgoingState::Allowed => None,
        };
        let state = if reason.is_some() {
            ToolResultStatus::Failed
        } else if matches!(behavior, MockBehavior::Pending) {
            ToolResultStatus::Processing
        } else {
            ToolResultStatus::Succeeded
        };
        let latest = receipt(
            tx,
            request,
            state,
            reason.or(if state == ToolResultStatus::Processing {
                Some("AWAITING_BANK_RESULT")
            } else {
                None
            }),
        )
        .await?;
        let first = if reason.is_none() && matches!(behavior, MockBehavior::LostReplyAfterCommit) {
            receipt(
                tx,
                request,
                ToolResultStatus::Processing,
                Some("OUTCOME_UNKNOWN"),
            )
            .await?
        } else {
            latest.clone()
        };
        let record =
            BusinessStore::record_submission(tx, owner, key, request, &first, &latest).await?;
        sqlx::query("INSERT INTO transfer_fund_orders_v1(owner_id,operation_id,payer_account_id,recipient_account_id,amount_cents,state) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(owner.as_str()).bind(request.operation_id.as_str()).bind(request.payer_account_id.as_str()).bind(recipient.as_str()).bind(amount).bind(result_state(state)).execute(&mut *tx).await?;
        if state == ToolResultStatus::Succeeded {
            settle(tx, owner, request, &recipient, &latest).await?;
        }
        sqlx::query(
            "UPDATE transfer_drafts_v1 SET submitted=TRUE WHERE owner_id=$1 AND draft_id=$2",
        )
        .bind(owner.as_str())
        .bind(request.draft_id.as_str())
        .execute(tx)
        .await?;
        Ok(record)
    }

    /// Authoritative later simulated-bank outcome. Expired draft is irrelevant
    /// AFTER acceptance; an unknown result keeps its hold until a definite result.
    pub async fn resolve(
        tx: &mut PgConnection,
        owner: &UserId,
        operation: &OperationId,
        resolution: MockResolution,
        outgoing: MockOutgoingState,
    ) -> Result<BusinessRecord, FundsError> {
        Self::resolve_inner(
            tx,
            owner,
            operation,
            resolution,
            OutgoingCheck::Fixed(outgoing),
        )
        .await
    }

    /// Re-read under both account locks before a PROCESSING order can settle.
    /// A terminal replay or definite bank failure does not need a card read.
    /// Unknown/error keeps the existing PROCESSING order and hold after rollback.
    pub async fn resolve_checked(
        tx: &mut PgConnection,
        owner: &UserId,
        operation: &OperationId,
        resolution: MockResolution,
        source: &mut dyn LockedOutgoingSource,
    ) -> Result<BusinessRecord, FundsError> {
        Self::resolve_inner(
            tx,
            owner,
            operation,
            resolution,
            OutgoingCheck::Current(source),
        )
        .await
    }

    async fn resolve_inner(
        tx: &mut PgConnection,
        owner: &UserId,
        operation: &OperationId,
        resolution: MockResolution,
        mut outgoing: OutgoingCheck<'_>,
    ) -> Result<BusinessRecord, FundsError> {
        lock(
            tx,
            &serde_json::json!(["transfer-business-operation-v1", owner, operation]).to_string(),
        )
        .await?;
        let record = Self::query(tx, owner, operation)
            .await?
            .ok_or(FundsError::NotFound)?;
        let desired = match resolution {
            MockResolution::Succeed => ToolResultStatus::Succeeded,
            MockResolution::Fail => ToolResultStatus::Failed,
        };
        if record.latest_receipt.status != ToolResultStatus::Processing {
            if record.latest_receipt.status == desired {
                return Ok(record);
            }
            return Err(FundsError::Conflict);
        }
        let recipient:String=sqlx::query_scalar("SELECT recipient_account_id FROM transfer_fund_orders_v1 WHERE owner_id=$1 AND operation_id=$2")
            .bind(owner.as_str()).bind(operation.as_str()).fetch_one(&mut *tx).await?;
        let recipient = ResourceId::parse(recipient).map_err(|_| FundsError::Invalid)?;
        lock_accounts(tx, &record.request.payer_account_id, &recipient).await?;
        // A definite failure skips card facts, but releasing its hold is still
        // a write and must stay inside the caller's protected transaction.
        outgoing.ensure_transaction(tx).await?;
        let state = match resolution {
            MockResolution::Fail => ToolResultStatus::Failed,
            MockResolution::Succeed => match outgoing
                .read(tx, owner, &record.request.payer_account_id)
                .await?
            {
                MockOutgoingState::Allowed => ToolResultStatus::Succeeded,
                MockOutgoingState::Blocked => ToolResultStatus::Failed,
                MockOutgoingState::Unknown => return Err(FundsError::OutgoingUnknown),
            },
        };
        let latest = receipt(
            tx,
            &record.request,
            state,
            if state == ToolResultStatus::Succeeded {
                None
            } else if matches!(resolution, MockResolution::Succeed) {
                Some("ACCOUNT_OUTGOING_BLOCKED")
            } else {
                Some("MOCK_BANK_DECLINED")
            },
        )
        .await?;
        // The hold remains counted until settlement has used it, then is released
        // atomically with the terminal state. Other operations' holds are intact.
        if state == ToolResultStatus::Succeeded {
            settle(tx, owner, &record.request, &recipient, &latest).await?;
        }
        sqlx::query(
            "UPDATE transfer_fund_orders_v1 SET state=$3 WHERE owner_id=$1 AND operation_id=$2",
        )
        .bind(owner.as_str())
        .bind(operation.as_str())
        .bind(result_state(state))
        .execute(&mut *tx)
        .await?;
        Ok(BusinessStore::observe_result(tx, owner, operation, &latest).await?)
    }

    /// Owner-scoped snapshot; holds are derived from authoritative fund orders,
    /// NEVER inferred from a response string or manufactured completed ledger.
    pub async fn funds(
        connection: &mut PgConnection,
        owner: &UserId,
        account: &ResourceId,
    ) -> Result<MockAccountFunds, FundsError> {
        let row=sqlx::query("SELECT booked_cents,(a.booked_cents::numeric=a.opening_cents::numeric+simulation_balance_net_v1(a.account_id)+COALESCE((SELECT sum(CASE WHEN l.direction='CREDIT' THEN l.amount_cents::numeric ELSE -l.amount_cents::numeric END) FROM transfer_ledger_v1 l WHERE l.account_id=a.account_id),0)) balanced,(SELECT COALESCE(sum(amount_cents),0)::bigint FROM transfer_fund_orders_v1 f WHERE f.payer_account_id=a.account_id AND f.state='PROCESSING') reserved FROM transfer_accounts_v1 a WHERE owner_id=$1 AND account_id=$2")
            .bind(owner.as_str()).bind(account.as_str()).fetch_optional(connection).await?.ok_or(FundsError::NotFound)?;
        if !row.try_get::<bool,_>("balanced")? {return Err(FundsError::Conflict)}
        let booked: i64 = row.try_get("booked_cents")?;
        let reserved: i64 = row.try_get("reserved")?;
        if reserved < 0 || reserved > booked {
            return Err(FundsError::Conflict);
        }
        Ok(MockAccountFunds {
            booked: decimal(booked)?,
            reserved: decimal(reserved)?,
            available: decimal(booked - reserved)?,
        })
    }

    pub async fn query(
        connection: &mut PgConnection,
        owner: &UserId,
        operation: &OperationId,
    ) -> Result<Option<BusinessRecord>, FundsError> {
        // One statement, one snapshot: do not straddle a concurrent resolution.
        let row=sqlx::query("SELECT b.original_key,b.request,b.first_receipt,b.latest_receipt,f.payer_account_id,f.amount_cents,f.state FROM transfer_business_orders_v1 b LEFT JOIN transfer_fund_orders_v1 f USING(owner_id,operation_id) WHERE b.owner_id=$1 AND b.operation_id=$2")
            .bind(owner.as_str()).bind(operation.as_str()).fetch_optional(connection).await?;
        if let Some(row) = row {
            let record = crate::business_store::decode(&row, owner)?;
            if record.request.operation_id != *operation
                || row.try_get::<Option<&str>, _>("payer_account_id")?
                    != Some(record.request.payer_account_id.as_str())
                || row.try_get::<Option<i64>, _>("amount_cents")?
                    != Some(cents(&record.request.amount)?)
                || row.try_get::<Option<&str>, _>("state")?
                    != Some(result_state(record.latest_receipt.status))
            {
                return Err(FundsError::Conflict);
            }
            return Ok(Some(record));
        }
        Ok(None)
    }

    /// Completed TRANSFER rows only, restricted to accounts owned by this user.
    /// A CREDIT row is not permission to read the sender's complete draft/order.
    pub async fn transactions(
        connection: &mut PgConnection,
        owner: &UserId,
    ) -> Result<Vec<MockTransferTransaction>, FundsError> {
        let rows=sqlx::query("SELECT l.* FROM transfer_ledger_v1 l JOIN transfer_accounts_v1 a USING(account_id) WHERE a.owner_id=$1 ORDER BY l.transaction_id")
            .bind(owner.as_str()).fetch_all(&mut *connection).await?;
        let mut transactions=rows.into_iter()
            .map(|row| {
                let completed_at: String = row.try_get("completed_at")?;
                // Same completed-receipt time invariant as BusinessStore.
                // Never invent a time or silently drop a malformed ledger row.
                chrono::DateTime::parse_from_rfc3339(&completed_at)
                    .map_err(|_| FundsError::Invalid)?;
                let amount_cents: i64 = row.try_get("amount_cents")?;
                // Ledger entries, unlike balances, must be strictly positive.
                if amount_cents <= 0 {
                    return Err(FundsError::Conflict);
                }
                Ok(MockTransferTransaction {
                    transaction_id: format!(
                        "transfer-ledger-{}",
                        row.try_get::<i64, _>("transaction_id")?
                    ),
                    operation_id: OperationId::parse(row.try_get::<String, _>("operation_id")?)
                        .map_err(|_| FundsError::Invalid)?,
                    account_id: ResourceId::parse(row.try_get::<String, _>("account_id")?)
                        .map_err(|_| FundsError::Invalid)?,
                    card_id: None,
                    merchant_id: None,
                    transaction_type: "TRANSFER".into(),
                    direction: match row.try_get::<&str, _>("direction")? {
                        "DEBIT" => TransferDirection::Debit,
                        "CREDIT" => TransferDirection::Credit,
                        _ => return Err(FundsError::Invalid),
                    },
                    amount: decimal(amount_cents)?,
                    currency: CnyCurrency::Cny,
                    status: "COMPLETED".into(),
                    completed_at,
                })
            })
            .collect::<Result<Vec<_>,FundsError>>()?;
        let adjustments=sqlx::query("SELECT s.*,s.recorded_at::text completed_at,simulation_balance_net_v1(s.account_id)::text verified_net FROM simulation_balance_adjustments_v1 s JOIN transfer_accounts_v1 a USING(account_id) WHERE a.owner_id=$1 AND s.owner_id=$1 AND s.delta_cents<>0 ORDER BY s.recorded_at,s.adjustment_id").bind(owner.as_str()).fetch_all(connection).await?;
        for row in adjustments {
            let delta:i64=row.try_get("delta_cents")?;
            let recorded:chrono::DateTime<chrono::Utc>=row.try_get("recorded_at")?;
            let id:String=row.try_get("adjustment_id")?;
            transactions.push(MockTransferTransaction {transaction_id:format!("simulation-adjustment-{id}"),operation_id:OperationId::parse(id).map_err(|_|FundsError::Invalid)?,account_id:ResourceId::parse(row.try_get::<String,_>("account_id")?).map_err(|_|FundsError::Invalid)?,card_id:None,merchant_id:None,transaction_type:"SIMULATION_BALANCE_ADJUSTMENT".into(),direction:if delta>0{TransferDirection::Credit}else{TransferDirection::Debit},amount:decimal(delta.checked_abs().ok_or(FundsError::Invalid)?)?,currency:CnyCurrency::Cny,status:"COMPLETED".into(),completed_at:recorded.to_rfc3339()});
        }
        Ok(transactions)
    }

    /// Backend-only, unpaginated read of ALL transfer-owned outgoing fund orders
    /// for a trusted owner, including failed and unresolved orders on every date.
    /// One SQL statement supplies the latest business/funds snapshot. No draft
    /// or incoming ledger rows are counted. An error must NOT become an empty list.
    ///
    /// submitted_at is deliberately None: the trusted public adapter must bind
    /// the matching owner + FUNDS business operation to the foundation's immutable
    /// first_submission.recorded_at. Storage/settlement times are not substitutes.
    /// This is neither proof of global snapshot completeness nor a payment permit;
    /// identity, public time consistency and concurrency through commit remain
    /// the foundation caller's responsibility, using its existing connection.
    pub async fn daily_usage_facts(
        connection: &mut PgConnection,
        owner: &UserId,
    ) -> Result<Vec<crate::daily_usage::SubmittedTransferFact>, FundsError> {
        valid_id(owner.as_str())?;
        let rows = sqlx::query(
            "SELECT f.owner_id,f.operation_id,f.payer_account_id,f.amount_cents,f.state,
                    a.owner_id AS payer_owner_id,
                    b.original_key,b.request,b.first_receipt,b.latest_receipt
             FROM transfer_fund_orders_v1 f
             LEFT JOIN transfer_business_orders_v1 b USING(owner_id,operation_id)
             LEFT JOIN transfer_accounts_v1 a ON a.account_id=f.payer_account_id
             WHERE f.owner_id=$1 OR a.owner_id=$1 ORDER BY f.operation_id",
        )
        .bind(owner.as_str())
        .fetch_all(connection)
        .await?;
        rows.into_iter()
            .map(|row| {
                if row.try_get::<&str, _>("owner_id")? != owner.as_str()
                    || row.try_get::<Option<&str>, _>("payer_owner_id")? != Some(owner.as_str())
                {
                    return Err(FundsError::Conflict);
                }
                let record = crate::business_store::decode(&row, owner)?;
                if row.try_get::<&str, _>("operation_id")? != record.request.operation_id.as_str()
                    || row.try_get::<&str, _>("payer_account_id")?
                        != record.request.payer_account_id.as_str()
                    || row.try_get::<i64, _>("amount_cents")? != cents(&record.request.amount)?
                    || row.try_get::<&str, _>("state")?
                        != result_state(record.latest_receipt.status)
                {
                    return Err(FundsError::Conflict);
                }
                Ok(crate::daily_usage::SubmittedTransferFact {
                    owner_id: owner.clone(),
                    operation_id: record.request.operation_id,
                    payer_account_id: record.request.payer_account_id,
                    amount: record.request.amount,
                    submitted_at: None,
                    status: record.latest_receipt.status,
                })
            })
            .collect()
    }

    /// IDs and owner must be resolved by the trusted backend. None creates v1;
    /// Some(n) updates only the actual v n. A modification increments once;
    /// cancellation/expiry keeps the version. Expiry and stable IDs never change.
    pub async fn save_draft(
        tx: &mut PgConnection,
        owner: &UserId,
        expected: Option<u32>,
        draft: &StoredDraft,
    ) -> Result<(), FundsError> {
        validate_draft(draft)?;
        if draft.user_id != *owner {
            return Err(FundsError::Invalid);
        }
        lock(
            tx,
            &serde_json::json!(["transfer-draft-v1", owner, draft.draft_id]).to_string(),
        )
        .await?;
        let row = sqlx::query(
            "SELECT * FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$2 FOR UPDATE",
        )
        .bind(owner.as_str())
        .bind(draft.draft_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let now = now(tx).await?;
        match row {
            None => {
                if expected.is_some() || draft.plan_version != 1 {
                    return Err(FundsError::VersionConflict);
                }
                if now >= draft.expires_at
                    || !matches!(
                        draft.status,
                        DraftStatus::ReadyToConfirm | DraftStatus::NeedsInformation
                    )
                {
                    return Err(FundsError::DraftInactive);
                }
                sqlx::query("INSERT INTO transfer_drafts_v1(owner_id,draft_id,operation_id,task_id,business_version,expires_at,status,details) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
                    .bind(owner.as_str()).bind(draft.draft_id.as_str()).bind(draft.operation_id.as_str()).bind(draft.task_id.as_str())
                    .bind(i64::from(draft.plan_version)).bind(draft.expires_at as i64).bind(status(draft.status)).bind(details(draft)?).execute(&mut *tx).await?;
            }
            Some(row) => {
                let old = decode_draft(&row)?;
                if expected != Some(old.plan_version) {
                    return Err(FundsError::VersionConflict);
                }
                if row.try_get::<bool, _>("submitted")? {
                    return Err(FundsError::AlreadySubmitted);
                }
                if old == *draft {
                    return Ok(());
                }
                if old.operation_id != draft.operation_id
                    || old.task_id != draft.task_id
                    || old.expires_at != draft.expires_at
                {
                    return Err(FundsError::Invalid);
                }
                if matches!(old.status, DraftStatus::Cancelled | DraftStatus::Expired) {
                    return Err(FundsError::DraftInactive);
                }
                let terminal =
                    matches!(draft.status, DraftStatus::Cancelled | DraftStatus::Expired);
                if now >= old.expires_at && draft.status != DraftStatus::Expired {
                    return Err(FundsError::DraftInactive);
                }
                if draft.status == DraftStatus::Expired && now < old.expires_at {
                    return Err(FundsError::Invalid);
                }
                if terminal {
                    if draft.plan_version != old.plan_version || draft.details != old.details {
                        return Err(FundsError::VersionConflict);
                    }
                } else if Some(draft.plan_version) != old.plan_version.checked_add(1) {
                    return Err(FundsError::VersionConflict);
                }
                sqlx::query("UPDATE transfer_drafts_v1 SET business_version=$3,status=$4,details=$5 WHERE owner_id=$1 AND draft_id=$2")
                    .bind(owner.as_str()).bind(draft.draft_id.as_str()).bind(i64::from(draft.plan_version)).bind(status(draft.status)).bind(details(draft)?).execute(&mut *tx).await?;
            }
        }
        Ok(())
    }

    pub async fn draft(
        connection: &mut PgConnection,
        owner: &UserId,
        draft_id: &ResourceId,
    ) -> Result<StoredDraft, FundsError> {
        let row = sqlx::query("SELECT * FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$2")
            .bind(owner.as_str())
            .bind(draft_id.as_str())
            .fetch_optional(&mut *connection)
            .await?
            .ok_or(FundsError::NotFound)?;
        let mut draft = decode_draft(&row)?;
        if now(connection).await? >= draft.expires_at
            && !matches!(draft.status, DraftStatus::Cancelled | DraftStatus::Expired)
        {
            draft.status = DraftStatus::Expired;
        }
        Ok(draft)
    }

    /// Backend-only recovery read on the caller's transaction connection. Holds
    /// the existing draft advisory/row locks; caller retains them through its
    /// guarded lifecycle save and COMMIT. No permit or identity is created here.
    pub async fn draft_for_restore(
        tx: &mut PgConnection,
        owner: &UserId,
        draft_id: &ResourceId,
        expected_version: u32,
        payee_query: &str,
    ) -> Result<crate::DraftRestore, FundsError> {
        valid_id(owner.as_str())?;
        valid_id(draft_id.as_str())?;
        crate::FindPayeesInput {
            query: payee_query.into(),
        }
        .validate()
        .map_err(|_| FundsError::Invalid)?;
        lock(
            tx,
            &serde_json::json!(["transfer-draft-v1", owner, draft_id]).to_string(),
        )
        .await?;
        let row = sqlx::query(
            "SELECT * FROM transfer_drafts_v1 WHERE owner_id=$1 AND draft_id=$2 FOR UPDATE",
        )
        .bind(owner.as_str())
        .bind(draft_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(FundsError::NotFound)?;
        let draft = decode_draft(&row)?;
        if draft.plan_version != expected_version || expected_version == 0 {
            return Err(FundsError::VersionConflict);
        }
        if row.try_get::<bool, _>("submitted")? {
            return Err(FundsError::AlreadySubmitted);
        }
        if draft.status != DraftStatus::ReadyToConfirm || now(tx).await? >= draft.expires_at {
            return Err(FundsError::DraftInactive);
        }
        Ok(crate::DraftRestore {
            draft,
            expected_version,
            submitted: false,
            payee_query: payee_query.into(),
        })
    }
}

async fn request_locks(
    tx: &mut PgConnection,
    owner: &UserId,
    key: &IdempotencyKey,
    op: &OperationId,
) -> Result<(), FundsError> {
    let mut locks = [
        serde_json::json!(["transfer-business-operation-v1", owner, op]).to_string(),
        serde_json::json!(["transfer-business-key-v1", owner, key]).to_string(),
    ];
    locks.sort();
    for value in locks {
        lock(tx, &value).await?;
    }
    Ok(())
}
fn result_state(value: ToolResultStatus) -> &'static str {
    match value {
        ToolResultStatus::Succeeded => "SUCCEEDED",
        ToolResultStatus::Failed => "FAILED",
        ToolResultStatus::Processing => "PROCESSING",
    }
}
fn decimal(value: i64) -> Result<DecimalAmount, FundsError> {
    if value < 0 {
        return Err(FundsError::Conflict);
    }
    DecimalAmount::parse(format!("{}.{:02}", value / 100, value % 100))
        .map_err(|_| FundsError::Invalid)
}
fn check_request(draft: &StoredDraft, r: &MockTransferRequest) -> Result<(), FundsError> {
    if draft.plan_version != r.draft_version {
        return Err(FundsError::VersionConflict);
    }
    let d = draft.details.as_ref().ok_or(FundsError::DraftInactive)?;
    if draft.operation_id != r.operation_id
        || draft.task_id != r.task_id
        || d.account.account_id != r.payer_account_id
        || d.payee.payee_id != r.payee_id
        || d.amount != r.amount
        || d.currency != r.currency
        || d.remark != r.remark
    {
        return Err(FundsError::Invalid);
    }
    Ok(())
}
async fn lock_accounts(
    tx: &mut PgConnection,
    payer: &ResourceId,
    recipient: &ResourceId,
) -> Result<(), FundsError> {
    let rows=sqlx::query("SELECT account_id FROM transfer_accounts_v1 WHERE account_id=$1 OR account_id=$2 ORDER BY account_id FOR UPDATE")
        .bind(payer.as_str()).bind(recipient.as_str()).fetch_all(tx).await?;
    if rows.len() != 2 {
        return Err(FundsError::NotFound);
    }
    Ok(())
}
async fn receipt(
    tx: &mut PgConnection,
    r: &MockTransferRequest,
    state: ToolResultStatus,
    reason: Option<&str>,
) -> Result<MockTransferReceipt, FundsError> {
    let completed_at = if state == ToolResultStatus::Succeeded {
        Some(sqlx::query_scalar::<_,String>("SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')").fetch_one(tx).await?)
    } else {
        None
    };
    Ok(MockTransferReceipt {
        proposal_version: 1,
        task_id: r.task_id.clone(),
        operation_id: r.operation_id.clone(),
        draft_id: r.draft_id.clone(),
        draft_version: r.draft_version,
        payer_account_id: r.payer_account_id.clone(),
        payee_id: r.payee_id.clone(),
        amount: r.amount.clone(),
        currency: r.currency,
        status: state,
        reason_code: reason.map(str::to_owned),
        completed_at,
    })
}
async fn settle(
    tx: &mut PgConnection,
    owner: &UserId,
    r: &MockTransferRequest,
    recipient: &ResourceId,
    receipt: &MockTransferReceipt,
) -> Result<(), FundsError> {
    let amount = cents(&r.amount)?;
    let balances = SelfBalances::read(tx, owner, r, recipient).await?;
    // Exclude this pending operation's OWN hold; never consume another hold.
    let own_hold:i64=sqlx::query_scalar("SELECT CASE WHEN state='PROCESSING' THEN amount_cents ELSE 0 END FROM transfer_fund_orders_v1 WHERE owner_id=$1 AND operation_id=$2")
        .bind(owner.as_str()).bind(r.operation_id.as_str()).fetch_one(&mut *tx).await?;
    if balances
        .available
        .checked_add(own_hold)
        .ok_or(FundsError::Conflict)?
        < amount
    {
        return Err(FundsError::Conflict);
    }
    let credited = balances
        .credit
        .checked_add(amount)
        .ok_or(FundsError::Invalid)?;
    sqlx::query("UPDATE transfer_accounts_v1 SET booked_cents=booked_cents-$2 WHERE account_id=$1")
        .bind(r.payer_account_id.as_str())
        .bind(amount)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE transfer_accounts_v1 SET booked_cents=$2 WHERE account_id=$1")
        .bind(recipient.as_str())
        .bind(credited)
        .execute(&mut *tx)
        .await?;
    for (account, direction) in [(&r.payer_account_id, "DEBIT"), (recipient, "CREDIT")] {
        sqlx::query("INSERT INTO transfer_ledger_v1(business_owner_id,operation_id,account_id,direction,amount_cents,completed_at) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(owner.as_str()).bind(r.operation_id.as_str()).bind(account.as_str()).bind(direction).bind(amount).bind(receipt.completed_at.as_ref().ok_or(FundsError::Invalid)?).execute(&mut *tx).await?;
    }
    Ok(())
}
struct SelfBalances {
    available: i64,
    credit: i64,
}
impl SelfBalances {
    async fn read(
        tx: &mut PgConnection,
        owner: &UserId,
        r: &MockTransferRequest,
        recipient: &ResourceId,
    ) -> Result<Self, FundsError> {
        let funds = FundsStore::funds(tx, owner, &r.payer_account_id).await?;
        let credit: i64 =
            sqlx::query_scalar("SELECT booked_cents FROM transfer_accounts_v1 WHERE account_id=$1")
                .bind(recipient.as_str())
                .fetch_one(tx)
                .await?;
        Ok(Self {
            available: cents(&funds.available)?,
            credit,
        })
    }
}

async fn lock(tx: &mut PgConnection, value: &str) -> Result<(), FundsError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(value)
        .execute(tx)
        .await?;
    Ok(())
}
async fn now(tx: &mut PgConnection) -> Result<u64, FundsError> {
    let time: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch from clock_timestamp()))::bigint")
            .fetch_one(tx)
            .await?;
    u64::try_from(time).map_err(|_| FundsError::Invalid)
}
fn status(value: DraftStatus) -> &'static str {
    match value {
        DraftStatus::ReadyToConfirm => "READY",
        DraftStatus::NeedsInformation => "NEEDS_INFORMATION",
        DraftStatus::Expired => "EXPIRED",
        DraftStatus::Cancelled => "CANCELLED",
    }
}
fn details(draft: &StoredDraft) -> Result<Option<serde_json::Value>, FundsError> {
    draft
        .details
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| FundsError::Invalid)
}
fn valid_id(value: &str) -> Result<(), FundsError> {
    if value.is_empty()
        || value.len() > 128
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        Err(FundsError::Invalid)
    } else {
        Ok(())
    }
}
fn validate_draft(draft: &StoredDraft) -> Result<(), FundsError> {
    for id in [
        draft.user_id.as_str(),
        draft.draft_id.as_str(),
        draft.operation_id.as_str(),
        draft.task_id.as_str(),
    ] {
        valid_id(id)?;
    }
    if draft.plan_version == 0 || draft.expires_at == 0 || draft.expires_at > i64::MAX as u64 {
        return Err(FundsError::Invalid);
    }
    if draft.status == DraftStatus::ReadyToConfirm && draft.details.is_none() {
        return Err(FundsError::Invalid);
    }
    if let Some(d) = &draft.details {
        GetAccountBalanceOutput {
            account: d.account.clone(),
            available_balance: d.available_balance.clone(),
            as_of: d.balance_as_of.clone(),
        }
        .validate_for(&d.account.account_id)
        .map_err(|_| FundsError::Invalid)?;
        FindPayeesOutput {
            candidates: vec![d.payee.clone()],
        }
        .validate()
        .map_err(|_| FundsError::Invalid)?;
        if cents(&d.amount)? == 0
            || d.remark.chars().count() > 140
            || d.remark.chars().any(char::is_control)
        {
            return Err(FundsError::Invalid);
        }
    }
    Ok(())
}
fn cents(value: &safeflow_bank_gateway::DecimalAmount) -> Result<i64, FundsError> {
    i64::try_from(crate::mock_execution::amount_cents(value).map_err(|_| FundsError::Invalid)?)
        .map_err(|_| FundsError::Invalid)
}
fn decode_draft(row: &PgRow) -> Result<StoredDraft, FundsError> {
    let draft = StoredDraft {
        user_id: UserId::parse(row.try_get::<String, _>("owner_id")?)
            .map_err(|_| FundsError::Invalid)?,
        draft_id: ResourceId::parse(row.try_get::<String, _>("draft_id")?)
            .map_err(|_| FundsError::Invalid)?,
        operation_id: OperationId::parse(row.try_get::<String, _>("operation_id")?)
            .map_err(|_| FundsError::Invalid)?,
        task_id: TaskId::parse(row.try_get::<String, _>("task_id")?)
            .map_err(|_| FundsError::Invalid)?,
        plan_version: u32::try_from(row.try_get::<i64, _>("business_version")?)
            .map_err(|_| FundsError::Invalid)?,
        expires_at: u64::try_from(row.try_get::<i64, _>("expires_at")?)
            .map_err(|_| FundsError::Invalid)?,
        status: match row.try_get::<&str, _>("status")? {
            "READY" => DraftStatus::ReadyToConfirm,
            "NEEDS_INFORMATION" => DraftStatus::NeedsInformation,
            "EXPIRED" => DraftStatus::Expired,
            "CANCELLED" => DraftStatus::Cancelled,
            _ => return Err(FundsError::Invalid),
        },
        details: row
            .try_get::<Option<serde_json::Value>, _>("details")?
            .map(serde_json::from_value::<DraftDetails>)
            .transpose()
            .map_err(|_| FundsError::Invalid)?,
    };
    validate_draft(&draft)?;
    Ok(draft)
}
