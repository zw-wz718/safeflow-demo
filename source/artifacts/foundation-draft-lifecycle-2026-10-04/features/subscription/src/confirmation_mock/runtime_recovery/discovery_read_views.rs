//! Additional safe shared queries, over one already validated owner snapshot.
use super::*;
pub(super) fn identify(
    s: &SubscriptionReadSnapshotMock,
    context: &MockContext,
    input: &Snapshot,
) -> Result<Value> {
    let v = s.identify_subscriptions_mock(context, input)?;
    Ok(
        json!({"fictional":true,"matching_executed":v.matching_executed,"reason":v.reason,
        "groups":v.groups.iter().map(|g|json!({"selection_index":g.selection_index,"disposition":g.disposition,
            "reason":g.reason,"message":g.message,"charges":g.charges,"matching_cycles":g.matching_cycles,
            "service_display_name":null})).collect::<Vec<_>>()}),
    )
}
pub(super) fn date(
    s: &SubscriptionReadSnapshotMock,
    context: &MockContext,
    input: &Snapshot,
    index: Option<usize>,
    id: Option<&str>,
    now: &str,
) -> Result<Value> {
    let v = s.suggest_reminder_date_mock(context, input, index, id, now)?;
    Ok(json!({"fictional":true,"state":v.state,"reason":v.reason,
        "prediction":v.prediction.as_ref().map(|p|json!({"cycle":p.cycle,"expected_date":p.expected_date.to_string(),"actual_charge_count":p.actual_charge_count,"needs_user_confirmation":p.needs_user_confirmation})),
        "suggested_schedule":v.suggested_schedule.as_ref().map(schedule_view),"message":v.message}))
}
pub(super) fn details(s: &SubscriptionReadSnapshotMock, id: &str, now: &str) -> Result<Value> {
    let list = s.list_subscriptions_mock()?;
    let item = list
        .items
        .iter()
        .find(|i| i.subscription_id == id)
        .ok_or(RuntimeRecoveryIssue::NotFound)?;
    let pending = s.pending_confirmations_mock(now)?;
    Ok(
        json!({"fictional":true,"service_display_name":null,"current_state":item.current_state,
        "service_data_state":item.service_data_state,"message":item.message,
        "cancellation":business_view(s,&SharedBusinessReadQueryMock::CancellationStatus {subscription_id:id.into()})?,
        "reminder":business_view(s,&SharedBusinessReadQueryMock::ReminderPlan {subscription_id:id.into()})?,
        "pending_confirmations":pending.items.iter().filter(|p|p.subscription_id.as_deref()==Some(id))
            .map(|p|json!({"action":p.action,"state":p.state,"expires_at":p.expires_at})).collect::<Vec<_>>()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: &str = "2026-10-14T12:00:00+08:00";
    fn sample() -> crate::Sample {
        let v: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/discovery_ready_input.v1.json"
        )))
        .unwrap();
        serde_json::from_value(v["sample"].clone()).unwrap()
    }
    fn snapshot(empty: bool) -> SubscriptionReadSnapshotMock {
        let mut j = super::super::super::super::raw_records_tests::actual_fixture_journal();
        if empty {
            j.commands.clear();
        }
        SubscriptionReadSnapshotMock::from_stored_journal_mock(
            "user_demo",
            j.commands.len() as i64 + 1,
            value(&j).unwrap(),
        )
        .unwrap()
    }
    fn safe(v: &Value) {
        let text = v.to_string();
        for k in [
            "user_demo",
            "account_001",
            "fictional-session",
            "source_id",
            "merchant_id",
            "service_ref",
            "subscription_id",
            "matched_subscription_ids",
            "anchor_date",
            "period_index",
            "journal",
        ] {
            assert!(!text.contains(k), "{k}");
        }
    }
    #[test]
    fn shared_recognition_has_safe_candidates_without_new_registration() {
        let s = snapshot(true);
        let f = sample();
        let before = value(&s.journal).unwrap();
        let v = identify(&s, &f.trusted_context_fixture, &f.snapshot).unwrap();
        safe(&v);
        assert_eq!(v["matching_executed"], true);
        assert_eq!(v["groups"][0]["disposition"], "NOT_IN_LEDGER");
        assert_eq!(before, value(&s.journal).unwrap());
        assert!(s.list_subscriptions_mock().unwrap().items.is_empty());
    }
    #[test]
    fn partial_consumption_stops_shared_recognition() {
        let s = snapshot(true);
        let mut f = sample();
        f.snapshot.transactions_read.is_complete = false;
        let v = identify(&s, &f.trusted_context_fixture, &f.snapshot).unwrap();
        assert_eq!(v["matching_executed"], false);
        safe(&v);
    }
    #[test]
    fn shared_date_suggestion_is_safe_unconfirmed_and_does_not_save_settings() {
        let s = snapshot(true);
        let f = sample();
        let before = value(&s.journal).unwrap();
        let v = date(
            &s,
            &f.trusted_context_fixture,
            &f.snapshot,
            Some(0),
            None,
            NOW,
        )
        .unwrap();
        safe(&v);
        assert_eq!(v["state"], "READY");
        assert_eq!(v["prediction"]["needs_user_confirmation"], true);
        assert_eq!(v["suggested_schedule"]["days_before"], 3);
        assert_eq!(before, value(&s.journal).unwrap());
    }
    #[test]
    fn missing_or_expired_date_is_not_rolled_forward() {
        let s = snapshot(true);
        let mut f = sample();
        assert_eq!(
            date(&s, &f.trusted_context_fixture, &f.snapshot, None, None, NOW).unwrap()["state"],
            "NEEDS_INPUT"
        );
        assert_eq!(
            date(
                &s,
                &f.trusted_context_fixture,
                &f.snapshot,
                Some(0),
                None,
                "2026-10-21T12:00:00+08:00"
            )
            .unwrap()["state"],
            "NEEDS_INPUT"
        );
        f.snapshot.transactions[0].mock_service_ref = None;
        assert_eq!(
            date(
                &s,
                &f.trusted_context_fixture,
                &f.snapshot,
                Some(0),
                None,
                NOW
            )
            .unwrap()["state"],
            "NEEDS_INPUT"
        );
    }
    #[test]
    fn shared_discovery_rejects_foreign_context() {
        let s = snapshot(true);
        let mut f = sample();
        f.trusted_context_fixture.user_id = "other".into();
        assert!(identify(&s, &f.trusted_context_fixture, &f.snapshot).is_err());
        assert!(date(
            &s,
            &f.trusted_context_fixture,
            &f.snapshot,
            Some(0),
            None,
            NOW
        )
        .is_err());
    }
    #[test]
    fn details_are_from_one_snapshot_and_contain_only_related_pending() {
        let s = snapshot(false);
        let id = s.list_subscriptions_mock().unwrap().items[0]
            .subscription_id
            .clone();
        let before = value(&s.journal).unwrap();
        let v = details(&s, &id, NOW).unwrap();
        safe(&v);
        assert_eq!(v["current_state"], "ACTIVE");
        assert_eq!(v["cancellation"]["history"].as_array().unwrap().len(), 2);
        assert!(v["pending_confirmations"].as_array().unwrap().is_empty());
        assert_eq!(before, value(&s.journal).unwrap());
        assert!(details(&s, "unknown", NOW).is_err());
    }
    #[test]
    fn all_new_shared_read_commands_parse_and_reject_extra_fields() {
        let f = sample();
        for q in [
            json!({"kind":"IdentifySubscriptions","context":f.trusted_context_fixture,"input":f.snapshot}),
            json!({"kind":"SuggestReminderDate","context":f.trusted_context_fixture,"input":f.snapshot,"candidate_index":0,"subscription_id":null,"now":NOW}),
            json!({"kind":"SubscriptionDetails","subscription_id":"x","now":NOW}),
        ] {
            let _: SharedSubscriptionReadQueryMock = serde_json::from_value(q.clone()).unwrap();
            let mut bad = q;
            bad["send"] = json!(true);
            assert!(serde_json::from_value::<SharedSubscriptionReadQueryMock>(bad).is_err());
        }
    }
}
