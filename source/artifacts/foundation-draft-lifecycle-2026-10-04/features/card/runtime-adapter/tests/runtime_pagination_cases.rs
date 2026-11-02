mod support;

use safeflow_card_facts::account_service::{AccountDataSource, AccountPage};
use safeflow_card_facts::service::SourceUnavailable;
use safeflow_card_facts::{Account, Card, LockRecord, LockSource, LockState};
use safeflow_contracts::{SecurityLevel, SecurityOutcome};
use safeflow_runtime::{AuditEventKind, RuntimeError, RuntimeOutcome};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use support::{assembly, block_on, plan, session};

#[derive(Clone, Copy)]
enum Mode {
    Locked,
    SecondPageFailure,
    SecondPageChanged,
    SecondPageMissing,
    SecondPageForeign,
    LockBeforeExecution,
    FailBeforeExecution,
}

struct PagedSource {
    mode: Mode,
    starts: AtomicUsize,
    page_reads: AtomicUsize,
}

impl AccountDataSource for PagedSource {
    fn read_page(
        &self,
        user: &str,
        account: &str,
        cursor: Option<&str>,
        expected: Option<&str>,
    ) -> Result<AccountPage, SourceUnavailable> {
        assert_eq!((user, account), ("U1", "A"));
        self.page_reads.fetch_add(1, Ordering::SeqCst);
        let second = cursor.is_some();
        if !second {
            self.starts.fetch_add(1, Ordering::SeqCst);
        }
        let execution = self.starts.load(Ordering::SeqCst) >= 3;
        let revision = if matches!(self.mode, Mode::LockBeforeExecution) && execution {
            "r2"
        } else {
            "r1"
        };
        if second {
            assert_eq!(cursor, Some("page-2"));
            assert_eq!(expected, Some(revision));
            if matches!(self.mode, Mode::SecondPageFailure)
                || matches!(self.mode, Mode::FailBeforeExecution) && execution
            {
                return Err(SourceUnavailable);
            }
        } else {
            assert!(expected.is_none());
        }
        let locked = !matches!(self.mode, Mode::LockBeforeExecution) || execution;
        let missing = second && matches!(self.mode, Mode::SecondPageMissing);
        Ok(AccountPage {
            account: Account {
                id: "A".into(),
                user_id: if second && matches!(self.mode, Mode::SecondPageForeign) {
                    "U2"
                } else {
                    "U1"
                }
                .into(),
            },
            revision: if second && matches!(self.mode, Mode::SecondPageChanged) {
                "r2"
            } else {
                revision
            }
            .into(),
            total_cards: 2,
            total_locks: usize::from(locked),
            cards: if missing {
                vec![]
            } else {
                vec![Card {
                    id: if second { "K2" } else { "K1" }.into(),
                    account_id: "A".into(),
                    last_four: "1234".into(),
                }]
            },
            locks: if second && locked {
                vec![LockRecord {
                    id: "L1".into(),
                    card_id: "K2".into(),
                    source: LockSource::RiskControl,
                    state: LockState::Active,
                }]
            } else {
                vec![]
            },
            next_cursor: if second { None } else { Some("page-2".into()) },
        })
    }

    fn read_current_revision(&self, _: &str, _: &str) -> Result<String, SourceUnavailable> {
        Ok(if matches!(self.mode, Mode::LockBeforeExecution)
            && self.starts.load(Ordering::SeqCst) >= 3
        {
            "r2"
        } else {
            "r1"
        }
        .into())
    }
}

fn source(mode: Mode) -> Arc<PagedSource> {
    Arc::new(PagedSource {
        mode,
        starts: AtomicUsize::new(0),
        page_reads: AtomicUsize::new(0),
    })
}

fn result_data(result: RuntimeOutcome) -> Value {
    let RuntimeOutcome::Completed { mut results, .. } = result else {
        panic!("expected completed read");
    };
    results.remove(0).data
}

#[test]
fn runtime_collects_the_second_pages_lock_on_each_of_its_three_reads() {
    let source = source(Mode::Locked);
    let (app, _) = assembly(source.clone(), SecurityLevel::Green);
    let data = result_data(
        block_on(
            app.runtime()
                .execute(plan(json!({"account_id":"A"})), session()),
        )
        .unwrap(),
    );
    assert_eq!(data["cards"][0]["active_locks"], json!([]));
    assert_eq!(data["active_card_lock_ids"], json!(["L1"]));
    assert_eq!(data["card_lock_restrictions"]["all_outgoing"], true);
    assert_eq!(source.page_reads.load(Ordering::SeqCst), 6);
    assert_eq!(source.starts.load(Ordering::SeqCst), 3);
}

fn assert_precheck_stops(mode: Mode) {
    let source = source(mode);
    let (app, audit) = assembly(source.clone(), SecurityLevel::Green);
    assert_eq!(
        block_on(
            app.runtime()
                .execute(plan(json!({"account_id":"A"})), session())
        ),
        Err(RuntimeError::SecurityGate(SecurityOutcome::Blocked))
    );
    assert_eq!(source.page_reads.load(Ordering::SeqCst), 2);
    assert!(!audit
        .events()
        .unwrap()
        .iter()
        .any(|event| event.kind == AuditEventKind::ToolStarted));
}

#[test]
fn failure_on_second_page_stops_before_the_tool() {
    assert_precheck_stops(Mode::SecondPageFailure);
}

#[test]
fn changed_revision_on_second_page_stops_before_the_tool() {
    assert_precheck_stops(Mode::SecondPageChanged);
}

#[test]
fn missing_card_on_second_page_cannot_be_a_partial_success() {
    assert_precheck_stops(Mode::SecondPageMissing);
}

#[test]
fn changed_owner_on_second_page_cannot_leak_foreign_card_facts() {
    assert_precheck_stops(Mode::SecondPageForeign);
}

#[test]
fn a_lock_added_after_both_policy_checks_is_visible_in_the_actual_result() {
    let source = source(Mode::LockBeforeExecution);
    let (app, _) = assembly(source.clone(), SecurityLevel::Green);
    let data = result_data(
        block_on(
            app.runtime()
                .execute(plan(json!({"account_id":"A"})), session()),
        )
        .unwrap(),
    );
    assert_eq!(data["revision"], "r2");
    assert_eq!(data["active_card_lock_ids"], json!(["L1"]));
    assert_eq!(data["card_lock_restrictions"]["transfer"], true);
    assert_eq!(source.page_reads.load(Ordering::SeqCst), 6);
}

#[test]
fn late_page_failure_after_policy_passes_does_not_complete_the_task() {
    let source = source(Mode::FailBeforeExecution);
    let (app, audit) = assembly(source.clone(), SecurityLevel::Green);
    assert!(block_on(
        app.runtime()
            .execute(plan(json!({"account_id":"A"})), session())
    )
    .is_err());
    assert_eq!(source.page_reads.load(Ordering::SeqCst), 6);
    let events = audit.events().unwrap();
    assert!(events
        .iter()
        .any(|event| event.kind == AuditEventKind::ToolStarted));
    assert!(!events.iter().any(|event| matches!(
        event.kind,
        AuditEventKind::ToolCompleted | AuditEventKind::TaskCompleted
    )));
}
