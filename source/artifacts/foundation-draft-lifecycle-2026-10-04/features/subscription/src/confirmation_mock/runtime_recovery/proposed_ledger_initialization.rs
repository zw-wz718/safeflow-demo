//! Explicit first-time fictional data only. Never initializes from a failed read,
//! overwrites an owner row, manages a transaction, or issues public permission.
use super::*;

/// Trusted backend evidence; not a customer's authorization or a public key.
/// Deserialized values are fully revalidated before lookup/insert.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerInitializationRequestMock {
    namespace: String,
    owner: String,
    initial: LedgerFixture,
}
#[derive(Debug, Serialize)]
pub struct LedgerInitializationPreviewMock {
    fictional: bool,
    saved: bool,
    known_empty: bool,
    entry_count: usize,
    review_required_count: usize,
    message: &'static str,
}
impl LedgerInitializationRequestMock {
    fn journal(&self, namespace: &str, owner: &str) -> WriteResult<Journal> {
        if self.namespace != namespace || self.owner != owner {
            return Err(ProposedLedgerWriteIssue::Conflict);
        }
        let journal = Journal {
            schema_version: 1,
            rule_version: crate::recognition::LOCAL_RULE_VERSION.into(),
            fictional: true,
            production_registered: false,
            initial: self.initial.clone(),
            commands: vec![],
        };
        // Same ownership/completeness/schema/entry validation used by recovery.
        decode_journal(owner, 1, value(&journal)?)?;
        Ok(journal)
    }
    /// Preview only, including for a persisted/deserialized backend request.
    /// Missing service fields stay missing; their records need business review.
    pub fn preview_mock(&self) -> WriteResult<LedgerInitializationPreviewMock> {
        ProposedLedgerReaderMock::new(&self.namespace).map_err(ProposedLedgerWriteIssue::Read)?;
        self.journal(&self.namespace, &self.owner)?;
        Ok(LedgerInitializationPreviewMock {
            fictional: true,
            saved: false,
            known_empty: self.initial.entries.is_empty(),
            entry_count: self.initial.entries.len(),
            review_required_count: self
                .initial
                .entries
                .iter()
                .filter(|e| {
                    e.status == LedgerStatus::Error
                        || [&e.source_id, &e.merchant_id, &e.service_ref]
                            .iter()
                            .any(|f| f.is_none())
                })
                .count(),
            message:
                "已核对这份虚构初始资料；尚未保存。缺失服务资料保留待核对，不构成登记或发送授权。",
        })
    }
}
impl ProposedLedgerStoreMock {
    pub fn prepare_initialization_mock(
        &self,
        trusted_owner: &str,
        initial: LedgerFixture,
    ) -> WriteResult<LedgerInitializationRequestMock> {
        let request = LedgerInitializationRequestMock {
            namespace: self.namespace.clone(),
            owner: trusted_owner.into(),
            initial,
        };
        request.journal(&self.namespace, trusted_owner)?;
        Ok(request)
    }
    /// Caller owns commit/rollback. INSERT only: concurrent/different data never
    /// updates the existing row. Equal original initial facts are idempotent,
    /// even after registrations, settings, intents and reminders were appended.
    pub async fn initialize_on_connection_mock(
        &self,
        connection: &mut PgConnection,
        trusted_owner: &str,
        request: &LedgerInitializationRequestMock,
    ) -> WriteResult<PendingProposedLedgerWriteMock> {
        let journal = request.journal(&self.namespace, trusted_owner)?;
        let scope = database_scope(connection).await?;
        let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *connection)
            .await
            .map_err(|_| ProposedLedgerWriteIssue::Unavailable)?;
        let query=format!("INSERT INTO \"{}\".subscription_runtime_records(owner_id,storage_version,encoding_version,journal) VALUES($1,1,$2,$3) ON CONFLICT(owner_id) DO NOTHING",self.namespace);
        sqlx::query(&query)
            .bind(trusted_owner)
            .bind(FICTIONAL_JOURNAL_ENCODING_V1)
            .bind(value(&journal)?)
            .execute(&mut *connection)
            .await
            .map_err(|_| ProposedLedgerWriteIssue::Unavailable)?;
        let saved = self
            .lookup_initialization_on_connection_mock(connection, trusted_owner, request)
            .await?
            .ok_or(ProposedLedgerWriteIssue::CommitNotObserved)?;
        Ok(PendingProposedLedgerWriteMock {
            namespace: self.namespace.clone(),
            database_scope: scope,
            owner: trusted_owner.into(),
            writer_pid,
            expected: value(&saved.journal)?,
            fresh_reminder_reservation: false,
        })
    }
    /// Low-level read only. None means no original initialization observed, NOT
    /// an empty ledger and never permission to automatically retry initialization.
    /// Trusted assembly controls read isolation/current identity. Use the pending
    /// object's independent-backend verification before a new success response.
    pub async fn lookup_initialization_on_connection_mock(
        &self,
        connection: &mut PgConnection,
        trusted_owner: &str,
        request: &LedgerInitializationRequestMock,
    ) -> WriteResult<Option<SubscriptionReadSnapshotMock>> {
        let original = request.journal(&self.namespace, trusted_owner)?;
        let saved = match ProposedLedgerReaderMock::new(&self.namespace)
            .map_err(ProposedLedgerWriteIssue::Read)?
            .load_on_connection_mock(connection, trusted_owner)
            .await
        {
            Ok(s) => s,
            Err(ProposedLedgerReadIssue::NotFound) => return Ok(None),
            Err(e) => return Err(ProposedLedgerWriteIssue::Read(e)),
        };
        if value(&saved.journal.initial)? != value(&original.initial)? {
            return Err(ProposedLedgerWriteIssue::Conflict);
        }
        Ok(Some(saved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> LedgerFixture {
        super::super::super::raw_records_tests::actual_fixture_journal().initial
    }
    fn store() -> ProposedLedgerStoreMock {
        ProposedLedgerStoreMock::new("fixture_initialization").unwrap()
    }
    #[test]
    fn complete_empty_is_explicit_and_preview_is_not_a_save_receipt() {
        let v = value(
            &store()
                .prepare_initialization_mock("user_demo", fixture())
                .unwrap()
                .preview_mock()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["known_empty"], true);
        assert_eq!(v["saved"], false);
        assert_eq!(v["entry_count"], 0);
        assert!(!v.to_string().contains("user_demo"));
    }
    #[test]
    fn unavailable_partial_cursor_and_missing_snapshot_never_become_empty() {
        for change in [0, 1, 2, 3] {
            let mut f = fixture();
            match change {
                0 => f.read.state = ReadKind::Unavailable,
                1 => f.read.is_complete = false,
                2 => f.read.next_cursor = Some("more".into()),
                _ => f.snapshot_id = None,
            };
            assert!(store().prepare_initialization_mock("user_demo", f).is_err());
        }
    }
    #[test]
    fn owner_fictional_flags_schema_and_duplicate_entries_are_checked() {
        assert!(store()
            .prepare_initialization_mock("other", fixture())
            .is_err());
        for change in [0, 1, 2] {
            let mut f = fixture();
            match change {
                0 => f.fictional = false,
                1 => f.production_registered = true,
                _ => f.schema_version = 2,
            };
            assert!(store().prepare_initialization_mock("user_demo", f).is_err());
        }
        let mut f = fixture();
        let e = LedgerEntry {
            subscription_id: "fixture-1".into(),
            owner_user_id: "user_demo".into(),
            source_id: None,
            merchant_id: None,
            service_ref: None,
            status: LedgerStatus::Active,
        };
        f.entries = vec![e.clone(), e];
        assert!(store()
            .prepare_initialization_mock("user_demo", f.clone())
            .is_err());
        f.entries.pop();
        f.entries[0].owner_user_id = "other".into();
        assert!(store().prepare_initialization_mock("user_demo", f).is_err());
    }
    #[test]
    fn incomplete_service_fields_are_preserved_as_review_not_invented() {
        let mut f = fixture();
        f.entries.push(LedgerEntry {
            subscription_id: "fixture-1".into(),
            owner_user_id: "user_demo".into(),
            source_id: None,
            merchant_id: None,
            service_ref: None,
            status: LedgerStatus::Active,
        });
        let request = store().prepare_initialization_mock("user_demo", f).unwrap();
        let v = value(&request.preview_mock().unwrap()).unwrap();
        assert_eq!(v["known_empty"], false);
        assert_eq!(v["entry_count"], 1);
        assert_eq!(v["review_required_count"], 1);
        assert_eq!(
            value(&request).unwrap()["initial"]["entries"][0]["service_ref"],
            Value::Null
        );
    }
    #[test]
    fn deserialized_request_cannot_bypass_validation_or_change_namespace() {
        let r = store()
            .prepare_initialization_mock("user_demo", fixture())
            .unwrap();
        let mut v = value(&r).unwrap();
        v["initial"]["read"]["is_complete"] = Value::Bool(false);
        let r: LedgerInitializationRequestMock = serde_json::from_value(v).unwrap();
        assert!(r.preview_mock().is_err());
        let mut r = store()
            .prepare_initialization_mock("user_demo", fixture())
            .unwrap();
        r.namespace = "public".into();
        assert!(r.preview_mock().is_err());
        assert!(r.journal("different_scope", "user_demo").is_err());
    }
}
