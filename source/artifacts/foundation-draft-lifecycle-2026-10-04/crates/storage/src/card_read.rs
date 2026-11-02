//! Session-bound, process-local navigation over member-owned opaque SQL reads.
//! All member connections are READ ONLY. Auxiliary catalog observations use the
//! existing owner protection transaction; neither operation writes business rows.
use crate::{card_catalog::CardObservationProof, repository::guarded, *};
use safeflow_card_postgres_store::{
    card_selection::{CardSelectionIssue, ReadOnlyCardList, SelectedCard},
    freeze_completion::{FreezeUserState, FreezeUserView},
    protection_history::{
        ProtectionHistoryIssue, ReadOnlyCardProtectionHistory, SelectedProtectionRecord,
    },
};
use safeflow_contracts::{ResourceId, SessionRef};
use safeflow_runtime::{SessionBinding, SessionResolver};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Instant,
};

#[path = "card_feedback_read.rs"]
mod feedback;

const MAX_SESSIONS: usize = 64;
const MAX_CARDS: usize = 4096;
const MAX_RECORDS: usize = 8192;
struct CardRetained {
    account: ResourceId,
    proof: CardObservationProof,
    list: ReadOnlyCardList,
    selected: Option<SelectedCard>,
    history: Option<ReadOnlyCardProtectionHistory>,
    record: Option<SelectedProtectionRecord>,
}
enum Retained {
    Card(CardRetained),
    Feedback(Box<feedback::FeedbackRetained>),
}
impl Retained {
    fn into_card(self) -> Result<CardRetained, StorageError> {
        match self {
            Self::Card(card) => Ok(card),
            _ => Err(StorageError::RevisionConflict),
        }
    }
}
struct Slot {
    request_ref: String,
    binding: SessionBinding,
    ticket: String,
    touched: Instant,
    retained: Option<Retained>,
}
#[derive(Clone)]
pub struct CardReadService {
    repository: PgRepository,
    reader: DatabaseCardReader,
    sessions: Arc<dyn SessionResolver>,
    slots: Arc<Mutex<HashMap<SessionRef, Slot>>>,
    cancelled: Arc<Mutex<VecDeque<(SessionRef, String)>>>,
}
// A request owns its step before the first await. Failure, panic, timeout and
// dropped futures destroy that state; an old drop cannot delete a newer step.
struct RequestStep {
    service: CardReadService,
    session: SessionRef,
    binding: SessionBinding,
    ticket: String,
    armed: bool,
}
impl Drop for RequestStep {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(mut slots) = self.service.slots.lock() {
                if slots
                    .get(&self.session)
                    .is_some_and(|s| s.ticket == self.ticket)
                {
                    slots.remove(&self.session);
                }
            }
        }
    }
}
impl RequestStep {
    fn check(&self) -> Result<(), StorageError> {
        if self
            .service
            .sessions
            .resolve_binding(&self.session)
            .map_err(|_| StorageError::Session)?
            != self.binding
        {
            return Err(StorageError::Session);
        }
        let slots = self
            .service
            .slots
            .lock()
            .map_err(|_| StorageError::DependencyUnavailable)?;
        if !slots
            .get(&self.session)
            .is_some_and(|s| s.ticket == self.ticket && s.binding == self.binding)
        {
            return Err(StorageError::RevisionConflict);
        }
        Ok::<(), StorageError>(())
    }
    async fn observe(&self, account: &ResourceId) -> Result<CardObservationProof, StorageError> {
        guarded(
            &|| self.check(),
            self.service.reader.observe_for_navigation(
                &self.session,
                self.service.sessions.as_ref(),
                &self.binding,
                account,
            ),
        )
        .await
    }
    fn publish(
        mut self,
        retained: CardRetained,
        original: Option<FreezeUserView>,
    ) -> Result<Value, StorageError> {
        self.check()?;
        let view = retained.list.view();
        if view.cards.len() > MAX_CARDS {
            return Err(StorageError::DependencyUnavailable);
        }
        let cards: Vec<_> = view.cards.iter().map(|c|json!({
            "position":c.position,"last_four":c.last_four,"ambiguous_tail":c.last_four_is_ambiguous,
            "card_locked":c.card_has_active_locks,"public_reasons":c.public_reasons
        })).collect();
        let history = retained.history.as_ref().map(|h| {
            let h = h.view();
            json!({"last_four":h.last_four,"card_locked":h.card_has_active_locks,
                "account_outgoing_restricted":h.account_outgoing_restricted_by_card_locks,
                "message":h.message,"ordering_explanation":h.ordering_explanation,
                "locks":h.locks.iter().map(|l|json!({"position":l.position,"active":l.currently_active,
                    "public_reason":l.public_reason,"records":l.protection_records.iter().map(|r|json!({
                        "position":r.position,"original_action":r.original_action.explanation(),
                        "public_reason":r.public_reason})).collect::<Vec<_>>()})).collect::<Vec<_>>()})
        });
        let original = original.map(|v|json!({
            "state":match v.state { FreezeUserState::OriginalRecordedActive=>"RECORDED_ACTIVE",
                FreezeUserState::OriginalRecordedInactive=>"RECORDED_INACTIVE",
                FreezeUserState::NeedsReview=>"NEEDS_REVIEW",FreezeUserState::Unavailable=>"UNAVAILABLE" },
            "last_four":v.last_four,"account_outgoing_restricted":v.account_outgoing_restricted,
            "public_reasons":v.public_reasons,"message":v.message
        }));
        let response = json!({"contract_version":1,"mode":"PROJECT_SIMULATION","scope":"CARD_READ_ONLY",
            "step_ref":self.ticket,"cards":cards,
            "account_outgoing_restricted":view.account_outgoing_restricted_by_card_locks,
            "account_public_reasons":view.account_public_reasons,"history":history,"original":original,
            "unlock_instruction":view.unlock_instruction,"funds_authorized":false});
        self.check()?;
        let mut slots = self
            .service
            .slots
            .lock()
            .map_err(|_| StorageError::DependencyUnavailable)?;
        let slot = slots
            .get_mut(&self.session)
            .ok_or(StorageError::RevisionConflict)?;
        if slot.ticket != self.ticket {
            return Err(StorageError::RevisionConflict);
        }
        slot.retained = Some(Retained::Card(retained));
        self.armed = false;
        Ok(response)
    }
}
impl CardReadService {
    pub fn for_trusted_backend(
        repository: PgRepository,
        sessions: Arc<dyn SessionResolver>,
        source: Option<CardCatalogSourceBinding>,
    ) -> Self {
        Self {
            reader: DatabaseCardReader::for_trusted_backend(repository.clone(), source),
            repository,
            sessions,
            slots: Arc::new(Mutex::new(HashMap::new())),
            cancelled: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
    fn start(
        &self,
        session: &SessionRef,
        previous: Option<&str>,
        request_ref: &str,
    ) -> Result<(RequestStep, Option<Retained>), StorageError> {
        if uuid::Uuid::parse_str(request_ref).is_err() {
            return Err(StorageError::Invalid("invalid navigation request"));
        }
        let cancelled = self
            .cancelled
            .lock()
            .map_err(|_| StorageError::DependencyUnavailable)?;
        if cancelled
            .iter()
            .any(|(s, r)| s == session && r == request_ref)
        {
            return Err(StorageError::RevisionConflict);
        }
        let binding = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        if binding.binding_version == 0 {
            return Err(StorageError::Session);
        }
        let mut slots = self
            .slots
            .lock()
            .map_err(|_| StorageError::DependencyUnavailable)?;
        if let Some(previous) = previous {
            let slot = slots.get(session).ok_or(StorageError::RevisionConflict)?;
            if slot.binding != binding || slot.ticket != previous || slot.retained.is_none() {
                return Err(StorageError::RevisionConflict);
            }
        }
        let retained = slots.remove(session).and_then(|s| s.retained);
        if slots.len() >= MAX_SESSIONS {
            if let Some(oldest) = slots
                .iter()
                .min_by_key(|(_, s)| s.touched)
                .map(|(k, _)| k.clone())
            {
                slots.remove(&oldest);
            }
        }
        let ticket = uuid::Uuid::new_v4().to_string();
        slots.insert(
            session.clone(),
            Slot {
                request_ref: request_ref.into(),
                binding: binding.clone(),
                ticket: ticket.clone(),
                touched: Instant::now(),
                retained: None,
            },
        );
        Ok((
            RequestStep {
                service: self.clone(),
                session: session.clone(),
                binding,
                ticket,
                armed: true,
            },
            retained,
        ))
    }
    /// A fresh query/refresh explicitly invalidates every prior selection in
    /// this session, even when the new query fails or is cancelled during SQL.
    pub async fn query(
        &self,
        session: &SessionRef,
        account: &ResourceId,
        request_ref: &str,
    ) -> Result<Value, StorageError> {
        let (step, _) = self.start(session, None, request_ref)?;
        let proof = step.observe(account).await?;
        let guard = || step.check();
        let mut tx = guarded(&guard, self.repository.pool.begin()).await?;
        guarded(&guard, async {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED READ ONLY")
                .execute(&mut *tx)
                .await?;
            Ok::<(), StorageError>(())
        })
        .await?;
        let list = guarded(&guard, async {
            ReadOnlyCardList::read_on_connection(
                &mut tx,
                step.binding.user_id.as_str(),
                account.as_str(),
            )
            .await
            .map_err(selection_error)
        })
        .await?;
        guarded(&guard, tx.commit()).await?;
        if step.observe(account).await? != proof {
            return Err(StorageError::FactsConflict);
        }
        step.publish(
            CardRetained {
                account: account.clone(),
                proof,
                list,
                selected: None,
                history: None,
                record: None,
            },
            None,
        )
    }
    pub async fn select_card(
        &self,
        session: &SessionRef,
        previous: &str,
        position: usize,
        request_ref: &str,
    ) -> Result<Value, StorageError> {
        let (step, retained) = self.start(session, Some(previous), request_ref)?;
        let mut retained = retained
            .ok_or(StorageError::RevisionConflict)?
            .into_card()?;
        if step.observe(&retained.account).await? != retained.proof {
            return Err(StorageError::FactsConflict);
        }
        let selected = retained
            .list
            .select_position(Some(position))
            .map_err(selection_error)?;
        let guard = || step.check();
        let mut tx = guarded(&guard, self.repository.pool.begin()).await?;
        guarded(&guard, async {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED READ ONLY")
                .execute(&mut *tx)
                .await?;
            Ok::<(), StorageError>(())
        })
        .await?;
        let history = guarded(&guard, async {
            selected
                .read_protection_history_on_connection(
                    &mut tx,
                    step.binding.user_id.as_str(),
                    retained.account.as_str(),
                )
                .await
                .map_err(history_error)
        })
        .await?;
        guarded(&guard, tx.commit()).await?;
        if history.view().total_saved_protection_records > MAX_RECORDS {
            return Err(StorageError::DependencyUnavailable);
        }
        if step.observe(&retained.account).await? != retained.proof {
            return Err(StorageError::FactsConflict);
        }
        retained.selected = Some(selected);
        retained.history = Some(history);
        retained.record = None;
        step.publish(retained, None)
    }
    pub async fn select_record(
        &self,
        session: &SessionRef,
        previous: &str,
        position: usize,
        request_ref: &str,
    ) -> Result<Value, StorageError> {
        let (step, retained) = self.start(session, Some(previous), request_ref)?;
        let mut retained = retained
            .ok_or(StorageError::RevisionConflict)?
            .into_card()?;
        if step.observe(&retained.account).await? != retained.proof {
            return Err(StorageError::FactsConflict);
        }
        let record = retained
            .history
            .as_ref()
            .ok_or(StorageError::RevisionConflict)?
            .select_record(Some(position))
            .map_err(history_error)?;
        let guard = || step.check();
        let mut tx = guarded(&guard, self.repository.pool.begin()).await?;
        guarded(&guard, async {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED READ ONLY")
                .execute(&mut *tx)
                .await?;
            Ok::<(), StorageError>(())
        })
        .await?;
        let resolution = guarded(&guard, async {
            record
                .query_on_connection(
                    &mut tx,
                    step.binding.user_id.as_str(),
                    retained.account.as_str(),
                )
                .await
                .map_err(history_error)
        })
        .await?;
        let view = resolution.user_view();
        guarded(&guard, tx.commit()).await?;
        if step.observe(&retained.account).await? != retained.proof {
            return Err(StorageError::FactsConflict);
        }
        retained.record = Some(record);
        step.publish(retained, Some(view))
    }
    pub fn cancel(&self, session: &SessionRef, previous: &str) -> Result<(), StorageError> {
        let (step, _) = self.start(session, Some(previous), &uuid::Uuid::new_v4().to_string())?;
        drop(step);
        Ok::<(), StorageError>(())
    }
    /// A client nonce can only discard its own read; it cannot select or recover.
    pub fn cancel_request(
        &self,
        session: &SessionRef,
        request_ref: &str,
    ) -> Result<(), StorageError> {
        let binding = self
            .sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        if uuid::Uuid::parse_str(request_ref).is_err() {
            return Err(StorageError::Invalid("invalid navigation request"));
        }
        let mut cancelled = self
            .cancelled
            .lock()
            .map_err(|_| StorageError::DependencyUnavailable)?;
        if !cancelled
            .iter()
            .any(|(s, r)| s == session && r == request_ref)
        {
            if cancelled.len() >= 256 {
                cancelled.pop_front();
            }
            cancelled.push_back((session.clone(), request_ref.into()));
        }
        let mut slots = self
            .slots
            .lock()
            .map_err(|_| StorageError::DependencyUnavailable)?;
        if slots
            .get(session)
            .is_some_and(|s| s.binding == binding && s.request_ref == request_ref)
        {
            slots.remove(session);
        }
        Ok(())
    }
}
fn selection_error(e: CardSelectionIssue) -> StorageError {
    match e {
        CardSelectionIssue::Unavailable | CardSelectionIssue::ReadOnlyBoundaryRequired => {
            StorageError::DependencyUnavailable
        }
        CardSelectionIssue::ResourceUnavailable => StorageError::NotFound,
        _ => StorageError::FactsConflict,
    }
}
fn history_error(e: ProtectionHistoryIssue) -> StorageError {
    match e {
        ProtectionHistoryIssue::Unavailable
        | ProtectionHistoryIssue::ReadOnlyBoundaryRequired
        | ProtectionHistoryIssue::Readback(_) => StorageError::DependencyUnavailable,
        ProtectionHistoryIssue::ResourceUnavailable => StorageError::NotFound,
        _ => StorageError::FactsConflict,
    }
}
