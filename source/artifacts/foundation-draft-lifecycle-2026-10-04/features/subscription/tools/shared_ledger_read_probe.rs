//! Local, fictional database acceptance only. Owns its disposable fixture setup.
//! Uses actual foundation migrations/demo; never opens a public permit issuer.
use safeflow_contracts::{ResourceId, UserId};
use safeflow_storage::PgRepository;
use safeflow_subscription_evidence::{
    confirmation_mock::{
        ProposedLedgerReadIssue as Issue, ProposedLedgerReaderMock, RuntimeRecoveryIssue,
        SubscriptionReadSnapshotMock, FICTIONAL_JOURNAL_ENCODING_V1,
    },
    recognition::LOCAL_RULE_VERSION,
    Sample,
};
use safeflow_transfer_integration::shared_demo::{DemoRecipient, SharedDemoPlan};
use serde_json::{json, Map, Value};
use sqlx::{postgres::PgConnectOptions, Connection, Executor, PgConnection};
use std::{error::Error, str::FromStr};
type Result<T> = std::result::Result<T, Box<dyn Error>>;
#[track_caller]
fn ensure(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(format!(
            "ACCEPTANCE_ASSERTION_FAILED:{}",
            std::panic::Location::caller().line()
        )
        .into())
    }
}
fn initial(status: Option<&str>) -> Value {
    let entries = status.map(|s| vec![json!({"subscription_id":"fictional-subscription-1", "owner_user_id":"user_demo", "source_id":"fictional-hannah-ready-v1", "merchant_id":"fictional-hannah-video", "service_ref":"fictional-hannah-video-service", "status":s})]).unwrap_or_default();
    json!({"schema_version":1,"fictional":true,"production_registered":false,"owner_user_id":"user_demo","snapshot_id":"fictional-complete-1","read":{"state":"COMPLETE","is_complete":true,"next_cursor":null,"reason":null},"entries":entries})
}
fn journal(status: Option<&str>) -> Value {
    json!({"schema_version":1,"rule_version":LOCAL_RULE_VERSION,"fictional":true,"production_registered":false,"initial":initial(status),"commands":[]})
}
async fn connect(options: &PgConnectOptions) -> Result<PgConnection> {
    let mut conn = PgConnection::connect_with(options).await?;
    conn.execute("SET statement_timeout='15s'").await?;
    Ok(conn)
}
async fn snapshot(conn: &mut PgConnection, schema: &str) -> Result<Value> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname=$1 ORDER BY tablename",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await?;
    let mut values = Map::new();
    for t in tables {
        ensure(t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))?;
        let data:Value=sqlx::query_scalar(&format!("SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]'::jsonb) FROM \"{schema}\".\"{t}\" t")).fetch_one(&mut *conn).await?;
        values.insert(t, data);
    }
    Ok(Value::Object(values))
}
async fn seed(
    conn: &mut PgConnection,
    schema: &str,
    owner: &str,
    encoding: &str,
    data: Value,
) -> Result<()> {
    let version = data["commands"]
        .as_array()
        .ok_or("COMMAND_ARRAY_REQUIRED")?
        .len() as i64
        + 1;
    sqlx::query(&format!("INSERT INTO \"{schema}\".subscription_runtime_records(owner_id,storage_version,encoding_version,journal) VALUES($1,$2,$3,$4) ON CONFLICT(owner_id) DO UPDATE SET storage_version=EXCLUDED.storage_version,encoding_version=EXCLUDED.encoding_version,journal=EXCLUDED.journal"))
        .bind(owner).bind(version).bind(encoding).bind(data).execute(conn).await?;
    Ok(())
}
fn ddl(schema: &str) -> Result<String> {
    // Explicit TEST SETUP of the existing commented proposal, not a migration.
    let text = include_str!(env!("SAFEFLOW_SUBSCRIPTION_PROPOSED_SQL"));
    let body = text
        .split_once("/*")
        .ok_or("DDL_COMMENT_REQUIRED")?
        .1
        .split_once("*/")
        .ok_or("DDL_COMMENT_REQUIRED")?
        .0;
    ensure(
        body.matches("CREATE TABLE").count() == 1
            && body.contains("__APPROVED_SCHEMA__.subscription_runtime_records"),
    )?;
    Ok(body.replace("__APPROVED_SCHEMA__", &format!("\"{schema}\"")))
}
async fn discovery(
    conn: &mut PgConnection,
    reader: &ProposedLedgerReaderMock,
    sample: &Sample,
) -> Result<Value> {
    let r = reader
        .identify_on_connection_mock(conn, &sample.trusted_context_fixture, &sample.snapshot)
        .await
        .map_err(|e| format!("READ_FAILED:{e:?}"))?;
    Ok(serde_json::to_value(r)?)
}
async fn cases(
    conn: &mut PgConnection,
    options: &PgConnectOptions,
    schema: &str,
    decoy: &str,
    role: &str,
    sample: &Sample,
    foundation_before: &Value,
) -> Result<Vec<Value>> {
    let reader = ProposedLedgerReaderMock::new(schema).map_err(|e| format!("{e:?}"))?;
    let mut records = vec![];
    macro_rules! case {
        ($name:expr,$body:expr) => {{
            eprintln!("CASE_START:{}",$name);
            let before=snapshot(conn,schema).await?;
            let detail:Value=$body;
            ensure(before==snapshot(conn,schema).await?)?;
            let mut common=before;common.as_object_mut().ok_or("SNAPSHOT_OBJECT_REQUIRED")?.remove("subscription_runtime_records");
            ensure(&common==foundation_before)?;
            records.push(json!({"case":$name,"verified":true,"actual_postgresql":true,"fictional":true,"read_changed_no_rows":true,"foundation_rows_unchanged":true,"detail":detail}));
        }}
    }
    seed(
        conn,
        schema,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        journal(None),
    )
    .await?;
    case!("complete_empty_is_known_empty_and_allows_candidates", {
        let saved = reader
            .load_on_connection_mock(conn, "user_demo")
            .await
            .map_err(|e| format!("{e:?}"))?;
        ensure(
            saved
                .list_subscriptions_mock()
                .map_err(|e| format!("{e:?}"))?
                .items
                .is_empty(),
        )?;
        let r = discovery(conn, &reader, sample).await?;
        ensure(
            r["matching_executed"] == true
                && r["groups"].as_array().is_some_and(|v| v.len() == 1)
                && r["already_managed_count"] == 0,
        )?;
        json!({"known_empty":true,"new_candidate_preserved":true})
    });
    seed(
        conn,
        schema,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        journal(Some("ACTIVE")),
    )
    .await?;
    case!("active_exact_service_hidden_from_new_recommendations", {
        let r = discovery(conn, &reader, sample).await?;
        ensure(
            r["matching_executed"] == true
                && r["groups"] == json!([])
                && r["already_managed_count"] == 1,
        )?;
        json!({"already_managed_count":1,"new_recommendations":0})
    });
    for (status, name, reason, message) in [
        (
            "CANCELLING",
            "cancel_intent_keeps_charge_evidence",
            "CANCELLATION_INTENT_NEEDS_REVIEW",
            "未确认商户已取消",
        ),
        (
            "ERROR",
            "abnormal_registration_keeps_charge_evidence",
            "REGISTERED_ERROR_NEEDS_REVIEW",
            "已有登记记录异常",
        ),
    ] {
        seed(
            conn,
            schema,
            "user_demo",
            FICTIONAL_JOURNAL_ENCODING_V1,
            journal(Some(status)),
        )
        .await?;
        case!(name, {
            let r = discovery(conn, &reader, sample).await?;
            ensure(
                r["groups"][0]["reason"] == reason
                    && r["groups"][0]["disposition"] == "NEEDS_REVIEW"
                    && r["groups"][0]["charges"]
                        .as_array()
                        .is_some_and(|v| v.len() == 2)
                    && r["groups"][0]["message"]
                        .as_str()
                        .is_some_and(|s| s.contains(message)),
            )?;
            json!({"retained_charges":2,"merchant_cancellation_confirmed":false,"reason":reason})
        });
    }
    let mut other = journal(Some("ACTIVE"));
    other["initial"]["entries"][0]["service_ref"] = json!("fictional-different-service");
    seed(
        conn,
        schema,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        other,
    )
    .await?;
    case!("same_merchant_different_service_is_not_hidden", {
        let r = discovery(conn, &reader, sample).await?;
        ensure(
            r["already_managed_count"] == 0 && r["groups"].as_array().is_some_and(|g| g.len() == 1),
        )?;
        json!({"merchant_name_or_amount_not_used_as_identity":true})
    });
    let mut duplicate = journal(Some("ACTIVE"));
    let mut second = duplicate["initial"]["entries"][0].clone();
    second["subscription_id"] = json!("fictional-subscription-2");
    duplicate["initial"]["entries"]
        .as_array_mut()
        .unwrap()
        .push(second);
    seed(
        conn,
        schema,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        duplicate,
    )
    .await?;
    case!("multiple_existing_records_need_review_instead_of_hiding", {
        let r = discovery(conn, &reader, sample).await?;
        ensure(
            r["groups"][0]["reason"] == "MULTIPLE_LEDGER_RECORDS_NEED_REVIEW"
                && r["groups"][0]["charges"]
                    .as_array()
                    .is_some_and(|v| v.len() == 2),
        )?;
        json!({"needs_review":true,"retained_charges":2})
    });
    case!("missing_owner_is_not_empty_and_input_is_bound", {
        for owner in ["missing-owner", "user_demo' OR '1'='1"] {
            ensure(matches!(
                reader.load_on_connection_mock(conn, owner).await,
                Err(Issue::NotFound)
            ))?;
        }
        ensure(matches!(
            reader.load_on_connection_mock(conn, " ").await,
            Err(Issue::ResourceUnavailable)
        ))?;
        json!({"not_found_distinct_from_empty":true,"parameter_binding_verified":true})
    });
    for (name, bad) in [
        ("incomplete_ledger", {
            let mut j = journal(None);
            j["initial"]["read"]["state"] = json!("INCOMPLETE");
            j["initial"]["read"]["is_complete"] = json!(false);
            j
        }),
        ("contradictory_complete", {
            let mut j = journal(None);
            j["initial"]["read"]["next_cursor"] = json!("still-more");
            j
        }),
        ("missing_snapshot", {
            let mut j = journal(None);
            j["initial"]["snapshot_id"] = Value::Null;
            j
        }),
        ("foreign_entry", {
            let mut j = journal(Some("ACTIVE"));
            j["initial"]["entries"][0]["owner_user_id"] = json!("foreign-owner");
            j
        }),
        ("wrong_rule", {
            let mut j = journal(None);
            j["rule_version"] = json!("unknown");
            j
        }),
        ("not_fictional", {
            let mut j = journal(None);
            j["fictional"] = json!(false);
            j
        }),
        ("pretend_production", {
            let mut j = journal(None);
            j["production_registered"] = json!(true);
            j
        }),
        ("invalid_command", {
            let mut j = journal(None);
            j["commands"] = json!([{"kind":"UNKNOWN"}]);
            j
        }),
        ("unknown_field", {
            let mut j = journal(None);
            j["extra"] = json!(true);
            j
        }),
    ] {
        seed(
            conn,
            schema,
            "user_demo",
            FICTIONAL_JOURNAL_ENCODING_V1,
            bad,
        )
        .await?;
        case!(name, {
            ensure(matches!(
                reader.load_on_connection_mock(conn, "user_demo").await,
                Err(Issue::InvalidStoredRecord)
            ))?;
            ensure(matches!(
                reader
                    .identify_on_connection_mock(
                        conn,
                        &sample.trusted_context_fixture,
                        &sample.snapshot
                    )
                    .await,
                Err(Issue::InvalidStoredRecord)
            ))?;
            json!({"matching_blocked":true,"no_fallback_to_empty":true})
        });
    }
    seed(
        conn,
        schema,
        "user_demo",
        "unknown-or-production-encoding",
        journal(None),
    )
    .await?;
    case!("unknown_codec_rejected_without_removing_fictional_flags", {
        ensure(matches!(
            reader.load_on_connection_mock(conn, "user_demo").await,
            Err(Issue::UnsupportedEncoding)
        ))?;
        json!({"unsupported_encoding":true})
    });
    seed(
        conn,
        schema,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        journal(Some("ACTIVE")),
    )
    .await?;
    case!("explicit_namespace_ignores_decoy_search_path", {
        let old: String = sqlx::query_scalar("SHOW search_path")
            .fetch_one(&mut *conn)
            .await?;
        sqlx::query("SELECT set_config('search_path',$1,false)")
            .bind(decoy)
            .execute(&mut *conn)
            .await?;
        let r = discovery(conn, &reader, sample).await?;
        ensure(r["already_managed_count"] == 1 && r["groups"] == json!([]))?;
        sqlx::query("SELECT set_config('search_path',$1,false)")
            .bind(old)
            .execute(&mut *conn)
            .await?;
        json!({"decoy_namespace_not_read":true})
    });
    case!("caller_read_only_transaction_remains_owned_by_caller", {
        conn.execute("BEGIN READ ONLY").await?;
        let before: (String, Option<i64>) = sqlx::query_as(
            "SELECT current_setting('transaction_read_only'),txid_current_if_assigned()",
        )
        .fetch_one(&mut *conn)
        .await?;
        let saved = reader
            .load_on_connection_mock(conn, "user_demo")
            .await
            .map_err(|e| format!("{e:?}"))?;
        ensure(saved.version_mock() == 1)?;
        let after: (String, Option<i64>) = sqlx::query_as(
            "SELECT current_setting('transaction_read_only'),txid_current_if_assigned()",
        )
        .fetch_one(&mut *conn)
        .await?;
        ensure(before == after && after.0 == "on")?;
        conn.execute("ROLLBACK").await?;
        json!({"no_begin_commit_or_restart_in_adapter":true,"transaction_stayed_read_only":true})
    });
    case!(
        "select_only_role_reads_and_revoked_permission_is_unavailable",
        {
            let mut limited = connect(options).await?;
            limited
                .execute(format!("SET ROLE \"{role}\"").as_str())
                .await?;
            ensure(discovery(&mut limited, &reader, sample).await?["already_managed_count"] == 1)?;
            for sql in [format!("INSERT INTO \"{schema}\".subscription_runtime_records SELECT * FROM \"{schema}\".subscription_runtime_records"),format!("UPDATE \"{schema}\".subscription_runtime_records SET storage_version=storage_version"),format!("DELETE FROM \"{schema}\".subscription_runtime_records")] {
            let e=limited.execute(sql.as_str()).await.unwrap_err();
            ensure(e.as_database_error().and_then(|e|e.code()).as_deref()==Some("42501"))?;
        }
            conn.execute(
                format!(
                    "REVOKE SELECT ON \"{schema}\".subscription_runtime_records FROM \"{role}\""
                )
                .as_str(),
            )
            .await?;
            ensure(matches!(
                reader
                    .load_on_connection_mock(&mut limited, "user_demo")
                    .await,
                Err(Issue::Unavailable)
            ))?;
            limited.close().await?;
            json!({"select_only_verified":true,"write_statements_rejected":3,"revocation_not_empty":true,"database_role_is_not_authenticated_owner":true})
        }
    );
    case!("missing_table_is_unavailable_and_never_created", {
        let missing = ProposedLedgerReaderMock::new("not_provisioned_subscription")
            .map_err(|e| format!("{e:?}"))?;
        ensure(matches!(
            missing.load_on_connection_mock(conn, "user_demo").await,
            Err(Issue::Unavailable)
        ))?;
        let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname='not_provisioned_subscription')").fetch_one(&mut *conn).await?;
        ensure(!exists)?;
        json!({"no_automatic_schema_initialization":true})
    });
    case!(
        "invalid_consumption_stops_matching_without_hiding_unknowns",
        {
            let mut bad = sample.snapshot.clone();
            bad.transactions_read.is_complete = false;
            let r = reader
                .identify_on_connection_mock(conn, &sample.trusted_context_fixture, &bad)
                .await
                .map_err(|e| format!("{e:?}"))?;
            ensure(!r.matching_executed && r.groups.is_empty() && r.already_managed_count == 0)?;
            json!({"matching_executed":false,"unknown_is_not_normal":true})
        }
    );
    case!("proposal_constraints_reject_version_and_owner_mismatch", {
        for statement in [
            format!("UPDATE \"{schema}\".subscription_runtime_records SET storage_version=0 WHERE owner_id='user_demo'"),
            format!("UPDATE \"{schema}\".subscription_runtime_records SET storage_version=2 WHERE owner_id='user_demo'"),
            format!("UPDATE \"{schema}\".subscription_runtime_records SET journal=jsonb_set(journal,'{{initial,owner_user_id}}','\"foreign-owner\"'::jsonb) WHERE owner_id='user_demo'")
        ] {
            let e=conn.execute(statement.as_str()).await.unwrap_err();
            ensure(e.as_database_error().and_then(|e|e.code()).as_deref()==Some("23514"))?;
        }
        json!({"invalid_fixture_writes_rejected":3,"constraints_are_not_public_write_authorization":true})
    });
    case!("old_dedicated_mock_guard_remains_enabled", {
        ensure(matches!(
            SubscriptionReadSnapshotMock::load_on_connection_mock(conn, "user_demo").await,
            Err(RuntimeRecoveryIssue::DedicatedDatabaseRequired)
        ))?;
        json!({"old_guard_preserved":true,"formal_codec_adopted":false})
    });
    // One fixture-only concurrent update verifies fresh committed reads. This is
    // NOT a public write/CAS/immutability acceptance or business authorization.
    let before = snapshot(conn, schema).await?;
    let first = reader
        .load_on_connection_mock(conn, "user_demo")
        .await
        .map_err(|e| format!("{e:?}"))?;
    let mut writer = connect(options).await?;
    writer.execute("BEGIN").await?;
    let mut next = journal(Some("ACTIVE"));
    next["commands"] = json!([{"kind":"Restart"}]);
    seed(
        &mut writer,
        schema,
        "user_demo",
        FICTIONAL_JOURNAL_ENCODING_V1,
        next,
    )
    .await?;
    let uncommitted = reader
        .load_on_connection_mock(conn, "user_demo")
        .await
        .map_err(|e| format!("{e:?}"))?;
    ensure(
        first.version_mock() == 1
            && uncommitted.version_mock() == 1
            && before == snapshot(conn, schema).await?,
    )?;
    writer.execute("COMMIT").await?;
    writer.close().await?;
    let after_commit = snapshot(conn, schema).await?;
    let second = reader
        .load_on_connection_mock(conn, "user_demo")
        .await
        .map_err(|e| format!("{e:?}"))?;
    ensure(
        first.version_mock() == 1
            && second.version_mock() == 2
            && after_commit == snapshot(conn, schema).await?,
    )?;
    let mut common = after_commit;
    common
        .as_object_mut()
        .unwrap()
        .remove("subscription_runtime_records");
    ensure(&common == foundation_before)?;
    records.push(json!({"case":"committed_version_read_without_cache_or_dirty_reads","verified":true,"actual_postgresql":true,"fictional":true,"read_changed_no_rows":true,"foundation_rows_unchanged":true,"detail":{"before_commit_version":1,"after_commit_version":2,"old_snapshot_unchanged":true,"fixture_only_writer":true}}));
    Ok(records)
}
// Compile-time compatibility with the real sealed transaction type. This
// function is deliberately never called: no permit constructor is fabricated.
async fn borrowed_type_check(
    transaction: &mut safeflow_storage::PermitTransaction,
    reader: &ProposedLedgerReaderMock,
    owner: &str,
) -> std::result::Result<SubscriptionReadSnapshotMock, String> {
    let connection = transaction.connection().map_err(|e| format!("{e:?}"))?;
    reader
        .load_on_connection_mock(connection, owner)
        .await
        .map_err(|e| format!("{e:?}"))
}
#[tokio::main]
async fn main() -> Result<()> {
    let _ = borrowed_type_check;
    let url = std::env::var("SAFEFLOW_SUBSCRIPTION_READ_TEST_URL")?;
    let options = PgConnectOptions::from_str(&url)?;
    ensure(
        options.get_host() == "127.0.0.1"
            && options.get_database() == Some("safeflow_subscription_read_test"),
    )?;
    let schema = std::env::var("SAFEFLOW_SUBSCRIPTION_READ_SCHEMA")?;
    ensure(
        schema.starts_with("sub_read_check_")
            && schema.len() < 42
            && schema
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
    )?;
    let decoy = format!("{schema}_decoy");
    let role = format!("{schema}_select");
    let repo = PgRepository::connect(&url, &schema).await?;
    let result:Result<Value>=async {
        repo.migrate().await?;
        let plan=SharedDemoPlan::prepare(DemoRecipient {owner_id:UserId::parse("fixture_subscription_recipient")?,account_id:ResourceId::parse("fixture_subscription_account")?})?;
        repo.initialize_shared_demo(&plan).await?;
        let mut conn=connect(&options).await?;
        let versions:Vec<i64>=sqlx::query_scalar(&format!("SELECT version FROM \"{schema}\"._sqlx_migrations WHERE success ORDER BY version")).fetch_all(&mut conn).await?;
        ensure(versions==(1..=21).collect::<Vec<_>>())?;
        let before=snapshot(&mut conn,&schema).await?;
        conn.execute(ddl(&schema)?.as_str()).await?;
        conn.execute(format!("CREATE SCHEMA \"{decoy}\"").as_str()).await?;
        conn.execute(ddl(&decoy)?.as_str()).await?;
        seed(&mut conn,&decoy,"user_demo",FICTIONAL_JOURNAL_ENCODING_V1,journal(None)).await?;
        conn.execute(format!("CREATE ROLE \"{role}\" NOLOGIN; GRANT USAGE ON SCHEMA \"{schema}\" TO \"{role}\"; GRANT SELECT ON \"{schema}\".subscription_runtime_records TO \"{role}\"").as_str()).await?;
        let raw:Value=serde_json::from_str(include_str!(env!("SAFEFLOW_SUBSCRIPTION_DISCOVERY_FIXTURE")))?;
        let sample:Sample=serde_json::from_value(raw["sample"].clone())?;
        let checks=cases(&mut conn,&options,&schema,&decoy,&role,&sample,&before).await?;
        conn.close().await?;
        Ok(json!({"scope":"ACTUAL_FOUNDATION_PROPOSED_FICTIONAL_SUBSCRIPTION_READ","verified":true,"cases":checks,"migration_versions":versions,"actual_foundation_migrator_and_demo":true,"old_mock_schema_installed":false,"public_permit_transaction_executed":false,"formal_subscription_integrated":false,"formal_codec_adopted":false,"subscription_business_write_executed":false,"notification_sent":false,"merchant_cancellation_confirmed":false}))
    }.await;
    repo.close().await;
    let mut cleanup = connect(&options).await?;
    cleanup.execute(format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE; DROP SCHEMA IF EXISTS \"{decoy}\" CASCADE; DROP ROLE IF EXISTS \"{role}\"").as_str()).await?;
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_namespace WHERE nspname=$1 OR nspname=$2")
            .bind(&schema)
            .bind(&decoy)
            .fetch_one(&mut cleanup)
            .await?;
    ensure(remaining == 0)?;
    cleanup.close().await?;
    let mut report = result?;
    report["private_schemas_and_role_removed"] = json!(true);
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
