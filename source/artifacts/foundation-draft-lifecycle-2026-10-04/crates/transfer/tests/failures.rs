mod support;
use safeflow_bank_gateway::{BankError, BankGateway, FindPayeesOutput, GetAccountBalanceOutput};
use safeflow_contracts::{ContractFuture, ResourceId, UserId};
use safeflow_mock_bank::MockBankGateway;
use safeflow_transfer_integration::{
    DraftChanges, DraftError, DraftInput, DraftStatus, IntegrationError, PayeeSelection,
    TransferDrafts,
};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use support::{block_on, context, ManualClock};

// Fault injection at the external bank seam only. Normal responses come from
// the delivered formal MockBankGateway, not duplicated bank fixtures.
struct FaultBank {
    bank: MockBankGateway,
    fault: Arc<AtomicU8>,
    clock: ManualClock,
}
impl BankGateway for FaultBank {
    fn get_account_balance<'a>(
        &'a self,
        user: &'a UserId,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        Box::pin(async move {
            if self.fault.load(Ordering::SeqCst) == 1 {
                return Err(BankError::Unavailable);
            }
            let mut result = self.bank.get_account_balance(user, account).await?;
            match self.fault.load(Ordering::SeqCst) {
                3 => result.account.masked_account_no = "622200006628".into(),
                4 => result.account.account_id = ResourceId::parse("account_002").unwrap(),
                7 => result.as_of = "2026-02-30T12:00:00Z".into(),
                _ => {}
            }
            Ok(result)
        })
    }
    fn find_payees<'a>(
        &'a self,
        user: &'a UserId,
        query: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        Box::pin(async move {
            if self.fault.load(Ordering::SeqCst) == 2 {
                return Err(BankError::Unavailable);
            }
            let mut result = self.bank.find_payees(user, query).await?;
            match self.fault.load(Ordering::SeqCst) {
                5 => result.candidates.push(result.candidates[0].clone()),
                6 => self.clock.set(1300),
                _ => {}
            }
            Ok(result)
        })
    }
}
fn input() -> DraftInput {
    DraftInput {
        payer_account_id: Some(ResourceId::parse("account_001").unwrap()),
        payee: Some(PayeeSelection::Name("小李".into())),
        amount: Some("200.00".into()),
        ..Default::default()
    }
}
fn setup() -> (TransferDrafts<ManualClock>, Arc<AtomicU8>, ManualClock) {
    let fault = Arc::new(AtomicU8::new(0));
    let clock = ManualClock::new(1000);
    let bank = Arc::new(FaultBank {
        bank: MockBankGateway::new(),
        fault: fault.clone(),
        clock: clock.clone(),
    });
    (
        TransferDrafts::new(bank, clock.clone(), 300).unwrap(),
        fault,
        clock,
    )
}

#[test]
fn account_or_payee_query_failure_never_creates_a_draft_using_fallback_data() {
    let (mut drafts, fault, _) = setup();
    let ctx = context("user_001", "failed_creation");
    for mode in [1, 2] {
        fault.store(mode, Ordering::SeqCst);
        assert_eq!(
            block_on(drafts.create(&ctx, input(), "")).unwrap_err(),
            IntegrationError::Bank(BankError::Unavailable)
        );
    }
    fault.store(0, Ordering::SeqCst);
    assert_eq!(
        block_on(drafts.create(&ctx, input(), ""))
            .unwrap()
            .saved
            .unwrap()
            .plan_version,
        1
    );
}

#[test]
fn failed_refresh_preserves_history_but_does_not_report_the_requested_change_as_saved() {
    let (mut drafts, fault, _) = setup();
    let ctx = context("user_001", "failed_modification");
    let first = block_on(drafts.create(&ctx, input(), ""))
        .unwrap()
        .saved
        .unwrap();
    for mode in [1, 2] {
        fault.store(mode, Ordering::SeqCst);
        assert_eq!(
            block_on(drafts.modify(
                &ctx,
                &first.draft_id,
                1,
                DraftChanges {
                    input: DraftInput {
                        amount: Some("300.00".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            ))
            .unwrap_err(),
            IntegrationError::Bank(BankError::Unavailable)
        );
        assert_eq!(drafts.read(&ctx, &first.draft_id).unwrap(), first);
    }
    fault.store(0, Ordering::SeqCst);
    let second = block_on(drafts.modify(
        &ctx,
        &first.draft_id,
        1,
        DraftChanges {
            remark: Some("新备注".into()),
            ..Default::default()
        },
    ))
    .unwrap()
    .saved
    .unwrap();
    assert_eq!(second.plan_version, 2);
    assert_eq!(second.details.unwrap().amount.as_str(), "200.00");
}

#[test]
fn invalid_unmasked_wrong_account_duplicate_or_bad_timestamp_responses_are_rejected() {
    let (mut drafts, fault, _) = setup();
    let ctx = context("user_001", "invalid_response_task");
    for mode in [3, 4, 5, 7] {
        fault.store(mode, Ordering::SeqCst);
        assert_eq!(
            block_on(drafts.create(&ctx, input(), "")).unwrap_err(),
            IntegrationError::Bank(BankError::InvalidResponse)
        );
    }
}

#[test]
fn expiry_during_formal_async_queries_prevents_a_partial_modification() {
    let (mut drafts, fault, clock) = setup();
    let ctx = context("user_001", "slow_bank_task");
    let first = block_on(drafts.create(&ctx, input(), ""))
        .unwrap()
        .saved
        .unwrap();
    fault.store(6, Ordering::SeqCst);
    assert_eq!(
        block_on(drafts.modify(
            &ctx,
            &first.draft_id,
            1,
            DraftChanges {
                input: DraftInput {
                    amount: Some("300.00".into()),
                    ..Default::default()
                },
                ..Default::default()
            }
        ))
        .unwrap_err(),
        IntegrationError::Draft(DraftError::Expired)
    );
    clock.set(1000);
    let history = drafts.read(&ctx, &first.draft_id).unwrap();
    assert_eq!(history.status, DraftStatus::Expired);
    assert_eq!(history.plan_version, 1);
    assert_eq!(history.details.unwrap().amount.as_str(), "200.00");
}

#[test]
fn unavailable_bank_does_not_prevent_local_cancel_or_override_a_version_conflict() {
    let (mut drafts, fault, _) = setup();
    let ctx = context("user_001", "local_cancel_task");
    let first = block_on(drafts.create(&ctx, input(), ""))
        .unwrap()
        .saved
        .unwrap();
    fault.store(1, Ordering::SeqCst);
    assert_eq!(
        block_on(drafts.modify(&ctx, &first.draft_id, 0, DraftChanges::default())).unwrap_err(),
        IntegrationError::Draft(DraftError::VersionConflict {
            expected: 0,
            actual: 1
        })
    );
    assert_eq!(
        drafts.cancel(&ctx, &first.draft_id, 1).unwrap().status,
        DraftStatus::Cancelled
    );
}
