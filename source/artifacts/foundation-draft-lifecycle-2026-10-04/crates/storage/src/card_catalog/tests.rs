//! Isolated public-mechanism tests. These catalogs are NOT adopted demo content.
mod public_read;
use super::*;
use safeflow_bank_gateway::DecimalAmount;
use safeflow_contracts::SecurityError;
use safeflow_transfer_integration::funds_store::FundsStore;
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    RwLock,
};

struct Sessions(AtomicU64);
impl SessionResolver for Sessions {
    fn resolve(&self, s: &SessionRef) -> Result<UserId, SecurityError> {
        Ok(self.resolve_binding(s)?.user_id)
    }
    fn resolve_binding(&self, _: &SessionRef) -> Result<SessionBinding, SecurityError> {
        Ok(SessionBinding {
            user_id: UserId::parse("catalog_test_owner").unwrap(),
            binding_version: self.0.load(Ordering::SeqCst),
        })
    }
}
struct Provider {
    cards: RwLock<Vec<CatalogCard>>,
    revision: AtomicU64,
    known: AtomicBool,
    wrong_owner: AtomicBool,
    fail: AtomicBool,
    change_on_check: AtomicBool,
}
impl Provider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            cards: RwLock::new(vec![]),
            revision: AtomicU64::new(1),
            known: AtomicBool::new(true),
            wrong_owner: AtomicBool::new(false),
            fail: AtomicBool::new(false),
            change_on_check: AtomicBool::new(false),
        })
    }
    fn set(&self, cards: &[(&str, &str)]) {
        *self.cards.write().unwrap() = cards
            .iter()
            .map(|(id, tail)| CatalogCard {
                card_id: (*id).into(),
                last_four: (*tail).into(),
            })
            .collect();
        self.revision.fetch_add(1, Ordering::SeqCst);
    }
}
impl BackendCardCatalogProvider for Provider {
    fn authorized_catalog<'a>(
        &'a self,
        identity: &'a SessionBinding,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<Option<BackendCardCatalog>, StorageError>> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                return Err(StorageError::DependencyUnavailable);
            }
            if !self.known.load(Ordering::SeqCst) {
                return Ok(None);
            }
            Ok(Some(BackendCardCatalog {
                owner: if self.wrong_owner.load(Ordering::SeqCst) {
                    UserId::parse("foreign").unwrap()
                } else {
                    identity.user_id.clone()
                },
                account: account.clone(),
                source_revision: format!("revision_{}", self.revision.load(Ordering::SeqCst)),
                cards: self.cards.read().unwrap().clone(),
            }))
        })
    }
    fn current_revision<'a>(
        &'a self,
        _: &'a SessionBinding,
        _: &'a ResourceId,
    ) -> ContractFuture<'a, Result<String, StorageError>> {
        Box::pin(async move {
            if self.change_on_check.load(Ordering::SeqCst) {
                return Err(StorageError::FactsConflict);
            }
            Ok(format!("revision_{}", self.revision.load(Ordering::SeqCst)))
        })
    }
}
struct Db {
    repo: PgRepository,
    sql: PgPool,
    admin: PgPool,
    schema: String,
    sessions: Arc<Sessions>,
    provider: Arc<Provider>,
    session: SessionRef,
    account: ResourceId,
}
impl Db {
    async fn new() -> Self {
        let url =
            std::env::var("SAFEFLOW_TEST_DATABASE_URL").expect("isolated PG required; not skipped");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("PG unavailable");
        let schema = format!("sf_card_public_{}", uuid::Uuid::new_v4().simple());
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .unwrap();
        let path = schema.clone();
        let sql = PgPoolOptions::new()
            .max_connections(4)
            .after_connect(move |c, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path',$1,false)")
                        .bind(path)
                        .execute(c)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("schema unavailable");
        let repo = PgRepository::connect(&url, &schema).await.unwrap();
        repo.migrate().await.unwrap();
        let account = ResourceId::parse("catalog_test_account").unwrap();
        let mut tx = sql.begin().await.unwrap();
        FundsStore::provision_account(
            &mut tx,
            &UserId::parse("catalog_test_owner").unwrap(),
            &account,
            &DecimalAmount::parse("500.00").unwrap(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        Self {
            repo,
            sql,
            admin,
            schema,
            sessions: Arc::new(Sessions(AtomicU64::new(1))),
            provider: Provider::new(),
            session: SessionRef::parse("catalog_isolated_session").unwrap(),
            account,
        }
    }
    fn binding(&self) -> CardCatalogSourceBinding {
        CardCatalogSourceBinding::for_reviewed_backend_provider(
            "isolated_test_provider",
            "test_impl_1",
            self.provider.clone(),
        )
        .unwrap()
    }
    fn service(&self) -> CardCatalogService {
        CardCatalogService::for_trusted_backend(
            self.repo.clone(),
            self.sessions.clone(),
            Some(self.binding()),
        )
    }
    async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&self.sql)
            .await
            .unwrap()
    }
    async fn close(self) {
        self.repo.pool.close().await;
        self.sql.close().await;
        self.admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn migration_replay_and_public_privilege_boundary() {
    let db = Db::new().await;
    db.repo.migrate().await.unwrap();
    assert_eq!(db.count("card_account_versions_v1").await, 0);
    let funcs:Vec<(String,bool,Option<Vec<String>>,bool)>=sqlx::query_as("SELECT proname,prosecdef,proconfig,EXISTS(SELECT 1 FROM aclexplode(COALESCE(proacl,acldefault('f',proowner))) WHERE grantee=0 AND privilege_type='EXECUTE') FROM pg_proc WHERE pronamespace=current_schema()::regnamespace AND proname LIKE 'card_%'").fetch_all(&db.sql).await.unwrap();
    assert!(funcs.len() >= 9);
    for (name, definer, config, public_exec) in funcs {
        assert!(!definer, "{name}");
        assert!(!public_exec, "{name}");
        assert!(
            config
                .unwrap()
                .iter()
                .any(|s| s == &format!("search_path={}", db.schema)),
            "{name}"
        );
    }
    let public_grants:i64=sqlx::query_scalar("SELECT count(*) FROM information_schema.table_privileges WHERE table_schema=current_schema() AND table_name LIKE '%card%' AND grantee='PUBLIC'").fetch_one(&db.sql).await.unwrap();
    assert_eq!(public_grants, 0);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn unknown_source_is_not_empty_and_foreign_account_is_denied() {
    let db = Db::new().await;
    let missing =
        CardCatalogService::for_trusted_backend(db.repo.clone(), db.sessions.clone(), None);
    assert!(matches!(
        missing.initialize(&db.session, &db.account).await,
        Err(StorageError::DependencyUnavailable)
    ));
    db.provider.known.store(false, Ordering::SeqCst);
    assert!(matches!(
        db.service().initialize(&db.session, &db.account).await,
        Err(StorageError::DependencyUnavailable)
    ));
    db.provider.known.store(true, Ordering::SeqCst);
    db.provider.wrong_owner.store(true, Ordering::SeqCst);
    assert!(matches!(
        db.service().initialize(&db.session, &db.account).await,
        Err(StorageError::NotFound)
    ));
    db.provider.wrong_owner.store(false, Ordering::SeqCst);
    assert!(db
        .service()
        .initialize(&db.session, &ResourceId::parse("foreign_account").unwrap())
        .await
        .is_err());
    assert_eq!(db.count("card_account_versions_v1").await, 0);
    db.service()
        .initialize(&db.session, &db.account)
        .await
        .unwrap();
    let facts = db
        .service()
        .observe(&db.session, &db.account)
        .await
        .unwrap();
    assert!(facts.cards.is_empty());
    assert!(!facts.outgoing_restricted());
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn complete_source_register_and_provenance_are_atomic() {
    let db = Db::new().await;
    db.provider.set(&[("catalog_card_a", "0022")]);
    let service = db.service();
    let revision = service.initialize(&db.session, &db.account).await.unwrap();
    assert_eq!(
        service.initialize(&db.session, &db.account).await.unwrap(),
        revision
    );
    assert_eq!(db.count("public_card_catalog_adoptions_v1").await, 1);
    db.provider.set(&[
        ("catalog_card_a", "0022"),
        ("catalog_card_b", "0033"),
        ("unmentioned_extra", "0044"),
    ]);
    assert!(service
        .register_mapping(
            &db.session,
            &db.account,
            &ResourceId::parse("catalog_card_b").unwrap()
        )
        .await
        .is_err());
    assert_eq!(db.count("card_catalog_v1").await, 1);
    db.provider
        .set(&[("catalog_card_a", "0022"), ("catalog_card_b", "0033")]);
    service
        .register_mapping(
            &db.session,
            &db.account,
            &ResourceId::parse("catalog_card_b").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        service
            .observe(&db.session, &db.account)
            .await
            .unwrap()
            .cards
            .len(),
        2
    );
    assert_eq!(db.count("public_card_catalog_adoptions_v1").await, 2);
    assert!(sqlx::query("DELETE FROM public_card_catalog_adoptions_v1")
        .execute(&db.sql)
        .await
        .is_err());
    let replacement = CardCatalogSourceBinding::for_reviewed_backend_provider(
        "replacement",
        "test_impl_1",
        db.provider.clone(),
    )
    .unwrap();
    assert!(CardCatalogService::for_trusted_backend(
        db.repo.clone(),
        db.sessions.clone(),
        Some(replacement)
    )
    .initialize(&db.session, &db.account)
    .await
    .is_err());
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn source_changed_or_database_failed_rolls_back_catalog_and_adoption() {
    let db = Db::new().await;
    db.provider.set(&[("catalog_card_a", "0022")]);
    db.provider.change_on_check.store(true, Ordering::SeqCst);
    assert!(db
        .service()
        .initialize(&db.session, &db.account)
        .await
        .is_err());
    assert_eq!(db.count("card_catalog_v1").await, 0);
    assert_eq!(db.count("card_account_versions_v1").await, 0);
    db.provider.change_on_check.store(false, Ordering::SeqCst);
    db.sql.execute("CREATE FUNCTION fail_adoption() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected'; END $$; CREATE TRIGGER fail_adoption BEFORE INSERT ON public_card_catalog_adoptions_v1 FOR EACH ROW EXECUTE FUNCTION fail_adoption()").await.unwrap();
    assert!(db
        .service()
        .initialize(&db.session, &db.account)
        .await
        .is_err());
    assert_eq!(db.count("card_catalog_v1").await, 0);
    assert_eq!(db.count("public_card_catalog_adoptions_v1").await, 0);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn bare_catalog_is_not_adopted_and_tail_aba_never_resets_version() {
    let db = Db::new().await;
    let mut tx = db.sql.begin().await.unwrap();
    sqlx::query("SELECT txid_current()")
        .execute(&mut *tx)
        .await
        .unwrap();
    LockedCardAccount::acquire(&mut tx, "catalog_test_owner", db.account.as_str())
        .await
        .unwrap()
        .initialize_catalog(&[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(
        db.service().observe(&db.session, &db.account).await,
        Err(StorageError::DependencyUnavailable)
    ));
    db.service()
        .initialize(&db.session, &db.account)
        .await
        .unwrap();
    db.provider.set(&[("catalog_card_a", "0022")]);
    db.service()
        .register_mapping(
            &db.session,
            &db.account,
            &ResourceId::parse("catalog_card_a").unwrap(),
        )
        .await
        .unwrap();
    let before = db
        .service()
        .observe(&db.session, &db.account)
        .await
        .unwrap()
        .revision;
    db.sql.execute("UPDATE card_catalog_v1 SET last_four='9999'; UPDATE card_catalog_v1 SET last_four='0022'").await.unwrap();
    let after = db
        .service()
        .observe(&db.session, &db.account)
        .await
        .unwrap()
        .revision;
    assert_ne!(before, after);
    for sql in [
        "DELETE FROM card_catalog_v1",
        "DELETE FROM card_account_versions_v1",
        "UPDATE card_account_versions_v1 SET revision=1",
        "UPDATE transfer_accounts_v1 SET owner_id='foreign'",
        "UPDATE card_catalog_v1 SET card_id='different'",
    ] {
        assert!(db.sql.execute(sql).await.is_err(), "{sql}");
    }
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn session_version_changes_while_waiting_roll_back_initialization() {
    let db = Db::new().await;
    let mut blocker = db.sql.begin().await.unwrap();
    sqlx::query("SELECT account_id FROM transfer_accounts_v1 FOR UPDATE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let blocking_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let service = db.service();
    let session = db.session.clone();
    let account = db.account.clone();
    let work = tokio::spawn(async move { service.initialize(&session, &account).await });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waits: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid))",
        )
        .bind(blocking_pid)
        .fetch_one(&db.sql)
        .await
        .unwrap();
        if waits > 0 {
            println!("catalog session guard: actual pg_blocking_pids wait observed");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no actual lock wait observed"
        );
        tokio::task::yield_now().await;
    }
    db.sessions.0.store(2, Ordering::SeqCst);
    blocker.commit().await.unwrap();
    assert!(matches!(work.await.unwrap(), Err(StorageError::Session)));
    assert_eq!(db.count("card_catalog_v1").await, 0);
    assert_eq!(db.count("card_account_versions_v1").await, 0);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn adopted_freeze_ddl_keeps_history_and_deferred_completeness() {
    let db = Db::new().await;
    db.provider.set(&[("catalog_card_a", "0022")]);
    db.service()
        .initialize(&db.session, &db.account)
        .await
        .unwrap();
    let mut incomplete = db.sql.begin().await.unwrap();
    sqlx::query("INSERT INTO card_locks_v1(lock_id,card_id,source,state) VALUES('incomplete','catalog_card_a','USER_SELF','ACTIVE')").execute(&mut *incomplete).await.unwrap();
    sqlx::query("INSERT INTO card_freeze_operations_v1(record_id,owner_id,account_id,operation_id,card_id,lock_id,source,reason,original_key) VALUES('incomplete_record','catalog_test_owner','catalog_test_account','incomplete_op','catalog_card_a','incomplete','USER_SELF','USER_REPORT_LOST','incomplete_key')").execute(&mut *incomplete).await.unwrap();
    assert!(incomplete.commit().await.is_err());
    assert_eq!(db.count("card_locks_v1").await, 0);
    db.sql.execute("SELECT card_save_freeze_v1('saved_lock','catalog_card_a','USER_SELF',NULL,NULL,true,'saved_record','catalog_test_owner','catalog_test_account','saved_op','USER_REPORT_LOST',NULL,NULL,'saved_key','saved_reason','CREATED')").await.unwrap();
    assert_eq!(db.count("card_freeze_receipts_v1").await, 1);
    assert!(db
        .service()
        .observe(&db.session, &db.account)
        .await
        .unwrap()
        .outgoing_restricted());
    for sql in ["DELETE FROM card_freeze_receipts_v1","UPDATE card_freeze_reasons_v1 SET reason='USER_SECURITY_CONCERN'","DELETE FROM card_freeze_operations_v1","DELETE FROM card_locks_v1","INSERT INTO card_locks_v1(lock_id,card_id,source,state) VALUES('duplicate','catalog_card_a','USER_SELF','ACTIVE')"] {assert!(db.sql.execute(sql).await.is_err(),"{sql}");}
    let a = db
        .service()
        .observe(&db.session, &db.account)
        .await
        .unwrap()
        .revision;
    db.sql
        .execute(
            "UPDATE card_locks_v1 SET state='RELEASED'; UPDATE card_locks_v1 SET state='ACTIVE'",
        )
        .await
        .unwrap();
    assert_ne!(
        a,
        db.service()
            .observe(&db.session, &db.account)
            .await
            .unwrap()
            .revision
    );
    db.close().await;
}
