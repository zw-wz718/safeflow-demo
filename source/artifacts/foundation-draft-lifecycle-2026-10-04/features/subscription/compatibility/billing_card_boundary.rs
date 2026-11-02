//! Isolated tests of pinned BILL-01 output and existing card/subscription readers.
//! Fixed demo identities and JSON are not authentication or completeness proof.
//! No normalizer, public contract, recognition input or funds command is added.

#[cfg(test)]
mod tests {
    use safeflow_billing_source_reader::{
        refresh_operation, BillingError, BillingReader, CnyAmount, DemoCase, LedgerStatus,
        ObservedOperation, OfflineFixtureSource, OperationStatus, OwnedTransaction, ReadOnlySource,
        SourceError, SourceTransaction, SubscriptionEvidence, TransferFixture, TrustedScope,
        DEMO_ACCOUNT, DEMO_USER, FIXTURE_SOURCE,
    };
    use safeflow_card_facts::account_service::{AccountDataSource, AccountPage, AccountQueryIssue};
    use safeflow_card_facts::service::SourceUnavailable;
    use safeflow_card_facts::transaction_reference::{
        TransactionCardQueryService, TransactionReferenceIssue, TrustedTransactionReference,
    };
    use safeflow_card_facts::{Account, Card, LockRecord, LockSource, LockState};
    use safeflow_subscription_evidence::{amount_minor_units, Transaction};
    use serde_json::{json, to_value};
    use std::cell::{Cell, RefCell};

    fn source(case: DemoCase) -> OfflineFixtureSource {
        let fixture =
            TransferFixture::from_json(include_str!(env!("SF_PINNED_TRANSFER_FIXTURE_PATH")))
                .expect("runner must provide the pinned upstream fixture");
        OfflineFixtureSource::new(fixture, case)
    }

    fn scope() -> TrustedScope {
        TrustedScope::from_verified_context(DEMO_USER, [DEMO_ACCOUNT]).unwrap()
    }

    // Local account facts only. They do not authenticate the fixed demo user.
    struct MockAccountDataSource {
        page: AccountPage,
        reads: Cell<usize>,
    }

    impl MockAccountDataSource {
        fn new(card_count: usize, locked: bool) -> Self {
            Self {
                page: AccountPage {
                    account: Account {
                        id: DEMO_ACCOUNT.into(),
                        user_id: DEMO_USER.into(),
                    },
                    revision: "isolated-card-facts-v1".into(),
                    total_cards: card_count,
                    total_locks: usize::from(locked),
                    cards: (0..card_count)
                        .map(|index| Card {
                            id: format!("demo-card-{index}"),
                            account_id: DEMO_ACCOUNT.into(),
                            last_four: "1234".into(),
                        })
                        .collect(),
                    locks: if locked {
                        vec![LockRecord {
                            id: "demo-lock".into(),
                            card_id: "demo-card-0".into(),
                            source: LockSource::RiskControl,
                            state: LockState::Active,
                        }]
                    } else {
                        vec![]
                    },
                    next_cursor: None,
                },
                reads: Cell::new(0),
            }
        }
    }

    impl AccountDataSource for &MockAccountDataSource {
        fn read_page(
            &self,
            _: &str,
            _: &str,
            cursor: Option<&str>,
            revision: Option<&str>,
        ) -> Result<AccountPage, SourceUnavailable> {
            assert!(cursor.is_none() && revision.is_none());
            self.reads.set(self.reads.get() + 1);
            Ok(self.page.clone())
        }

        fn read_current_revision(&self, _: &str, _: &str) -> Result<String, SourceUnavailable> {
            Ok(self.page.revision.clone())
        }
    }

    fn reference(tx: &SourceTransaction) -> TrustedTransactionReference<'_> {
        TrustedTransactionReference {
            transaction_id: &tx.transaction_id,
            account_id: Some(&tx.account_id),
            card_id: tx.card_id.as_deref(),
        }
    }

    #[test]
    fn successful_billing_null_card_is_not_guessed_from_one_or_several_cards() {
        let read = BillingReader::default()
            .read(&scope(), &source(DemoCase::Success))
            .unwrap();
        assert_eq!(read.entries.len(), 1);
        let tx = &read.entries[0].transaction;
        assert!(tx.card_id.is_none());
        for count in [1, 2] {
            let cards = MockAccountDataSource::new(count, false);
            let facts = TransactionCardQueryService::new(&cards)
                .query(DEMO_USER, reference(tx))
                .unwrap();
            assert_eq!(facts.account.cards.len(), count);
            assert!(facts.referenced_card.is_none());
            assert!(tx.card_id.is_none());
        }
    }

    #[test]
    fn lost_reply_reads_existing_completed_row_without_guessing_card() {
        let upstream = source(DemoCase::LostReply);
        assert_eq!(
            upstream.initial_operation().receipt.status,
            OperationStatus::Processing
        );
        let read = BillingReader::default().read(&scope(), &upstream).unwrap();
        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.entries[0].transaction.status, LedgerStatus::Completed);
        for count in [1, 2] {
            let cards = MockAccountDataSource::new(count, false);
            let facts = TransactionCardQueryService::new(&cards)
                .query(DEMO_USER, reference(&read.entries[0].transaction))
                .unwrap();
            assert!(facts.referenced_card.is_none());
        }
    }

    #[test]
    fn querying_success_or_lost_reply_preserves_existing_ledger_rows() {
        for case in [DemoCase::Success, DemoCase::LostReply] {
            let upstream = source(case);
            let mut reader = BillingReader::default();
            let before = reader.read(&scope(), &upstream).unwrap();
            let initial = upstream.initial_operation();
            let refreshed = refresh_operation(&scope(), &upstream, &initial).unwrap();
            assert_eq!(
                refreshed.last_known.receipt.status,
                OperationStatus::Succeeded
            );
            assert_eq!(
                refreshed.last_known.receipt.operation_id,
                initial.receipt.operation_id
            );
            assert!(refreshed.query_error.is_none());
            let after = reader.read(&scope(), &upstream).unwrap();
            assert_eq!(
                to_value(&before.entries).unwrap(),
                to_value(&after.entries).unwrap()
            );
            assert_eq!(after.added, 0);
        }
    }

    #[test]
    fn failed_or_still_processing_operations_have_no_completed_row_to_resolve() {
        for (case, expected) in [
            (DemoCase::Failure, OperationStatus::Failed),
            (DemoCase::Processing, OperationStatus::Processing),
        ] {
            let upstream = source(case);
            let read = BillingReader::default().read(&scope(), &upstream).unwrap();
            let cards = MockAccountDataSource::new(1, true);
            let service = TransactionCardQueryService::new(&cards);
            for entry in &read.entries {
                service
                    .query(DEMO_USER, reference(&entry.transaction))
                    .unwrap();
            }
            assert!(read.entries.is_empty());
            assert_eq!(cards.reads.get(), 0);
            let result =
                refresh_operation(&scope(), &upstream, &upstream.initial_operation()).unwrap();
            assert_eq!(result.last_known.receipt.status, expected);
            assert_eq!(
                result.next_read.is_some(),
                expected == OperationStatus::Processing
            );
        }
    }

    #[test]
    fn current_account_lock_does_not_rewrite_historical_completed_transaction() {
        let read = BillingReader::default()
            .read(&scope(), &source(DemoCase::Success))
            .unwrap();
        let tx = &read.entries[0].transaction;
        let before = to_value(tx).unwrap();
        let cards = MockAccountDataSource::new(2, true);
        let facts = TransactionCardQueryService::new(&cards)
            .query(DEMO_USER, reference(tx))
            .unwrap();
        assert!(facts.account.outgoing_restricted());
        assert!(facts.referenced_card.is_none());
        assert_eq!(tx.status, LedgerStatus::Completed);
        assert_eq!(to_value(tx).unwrap(), before);
    }

    struct UnavailableSource {
        queried_ids: RefCell<Vec<String>>,
    }

    impl ReadOnlySource for UnavailableSource {
        fn source_id(&self) -> &str {
            FIXTURE_SOURCE
        }
        fn read_completed_transactions(
            &self,
            _: &TrustedScope,
        ) -> Result<Vec<OwnedTransaction>, SourceError> {
            Err(SourceError::Unavailable)
        }
        fn query_operation(
            &self,
            _: &TrustedScope,
            operation_id: &str,
        ) -> Result<ObservedOperation, SourceError> {
            self.queried_ids.borrow_mut().push(operation_id.into());
            Err(SourceError::Unavailable)
        }
    }

    #[test]
    fn unavailable_source_is_not_empty_success_or_bank_failure() {
        let unavailable = UnavailableSource {
            queried_ids: RefCell::new(vec![]),
        };
        assert!(matches!(
            BillingReader::default().read(&scope(), &unavailable),
            Err(BillingError::Source(SourceError::Unavailable))
        ));
        for case in [DemoCase::Success, DemoCase::LostReply] {
            let initial = source(case).initial_operation();
            let view = refresh_operation(&scope(), &unavailable, &initial).unwrap();
            assert_eq!(view.last_known, initial);
            assert_eq!(view.query_error, Some(SourceError::Unavailable));
            assert_eq!(
                view.next_read.unwrap().business_operation_id,
                initial.receipt.operation_id
            );
            assert_eq!(
                unavailable.queried_ids.borrow().last(),
                Some(&initial.receipt.operation_id)
            );
        }
    }

    #[test]
    fn out_of_scope_account_is_rejected_by_billing_and_card_readers() {
        let wrong_scope =
            TrustedScope::from_verified_context(DEMO_USER, ["other-account"]).unwrap();
        let upstream = source(DemoCase::Success);
        assert!(matches!(
            BillingReader::default().read(&wrong_scope, &upstream),
            Err(BillingError::Source(SourceError::ResourceNotAccessible))
        ));
        let read = BillingReader::default().read(&scope(), &upstream).unwrap();
        let cards = MockAccountDataSource::new(1, false);
        let wrong_reference = TrustedTransactionReference {
            account_id: Some("other-account"),
            ..reference(&read.entries[0].transaction)
        };
        assert_eq!(
            TransactionCardQueryService::new(&cards).query(DEMO_USER, wrong_reference),
            Err(TransactionReferenceIssue::AccountQuery(
                AccountQueryIssue::ResourceUnavailable
            ))
        );
    }

    #[test]
    fn zero_added_on_repeat_read_is_not_empty_consumption_or_complete_evidence() {
        let upstream = source(DemoCase::Success);
        let mut reader = BillingReader::default();
        let first = reader.read(&scope(), &upstream).unwrap();
        let second = reader.read(&scope(), &upstream).unwrap();
        assert_eq!(first.added, 1);
        assert_eq!(second.added, 0);
        assert_eq!(second.unchanged, 1);
        assert_eq!(
            to_value(&first.entries).unwrap(),
            to_value(&second.entries).unwrap()
        );
        assert_eq!(second.entries.len(), 1);
        assert_eq!(
            second.subscription_evidence,
            SubscriptionEvidence::Unavailable
        );
        assert_eq!(
            to_value(second.entries[0].consumption).unwrap(),
            json!("TRANSFER_EXCLUDED")
        );
    }

    #[test]
    fn nullable_presence_and_direction_conversion_do_not_supply_missing_category() {
        let read = BillingReader::default()
            .read(&scope(), &source(DemoCase::Success))
            .unwrap();
        let row = to_value(&read.entries[0].transaction).unwrap();
        assert!(row["card_id"].is_null() && row["merchant_id"].is_null());
        assert!(!row.as_object().unwrap().contains_key("category_code"));
        for field in ["card_id", "merchant_id"] {
            let mut omitted = row.clone();
            omitted.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<SourceTransaction>(omitted).is_err());
        }
        for (upstream, downstream) in [("DEBIT", "OUT"), ("CREDIT", "IN")] {
            let mut input = row.clone();
            input["direction"] = json!(upstream);
            assert!(serde_json::from_value::<SourceTransaction>(input.clone()).is_ok());
            assert!(serde_json::from_value::<Transaction>(input.clone()).is_err());
            input["direction"] = json!(downstream);
            let error = serde_json::from_value::<Transaction>(input).unwrap_err();
            assert!(error.to_string().contains("category_code"));
        }
    }

    #[test]
    fn leading_zero_amount_difference_is_observed_without_normalizing_it() {
        let source_amount: CnyAmount = serde_json::from_value(json!("0200.00")).unwrap();
        assert_eq!(source_amount.as_str(), "0200.00");
        assert_eq!(amount_minor_units(source_amount.as_str()), None);
        assert_eq!(amount_minor_units("200.00"), Some(20_000));
    }
}
