use super::*;
fn legacy() -> TaskId { TaskId::parse(format!("bank_chat_{}_1", uuid::Uuid::new_v4())).unwrap() }
fn args() -> Value { json!({"payer_account_id":"account_001","payee_id":"payee_008","amount":"200.00","currency":"CNY","remark":"","replaces":null}) }
#[tokio::test]
#[ignore="new owned PostgreSQL only; original request proof, no execution"]
async fn orphan_chat_proof_retires_creation_and_preserves_all_existing_tasks() {
 let db=Db::new().await;let owner=safeflow_contracts::UserId::parse("user_001").unwrap();
 let base=db.confirmed().await;
 let id=legacy();
 let proof=db.repository.uncreated_chat_request_for_backend(&owner,&id,false).await.unwrap();
 assert_eq!(proof["proof_source"],"RETIRED_REQUEST_NAMESPACE");
 assert_eq!(proof["payment_cancelled"],false);
 assert_eq!(proof["context_ref"],db.repository.bank_recovery_for_backend(&owner).await.unwrap()["context_ref"]);
 assert_eq!(db.service().create(&session(),&id,args(),7).await.err(),Some(StorageError::IdConflict));
 let mut plan=base.task.plan.clone();plan.task_id=id.clone();
 assert_eq!(db.repository.create_task(&owner,plan,7,&|| Ok(())).await.err(),Some(StorageError::IdConflict));
 // Historical legacy tasks (including another owner) must never acquire proof.
 for who in ["user_001","user_002"] {
  let existing=legacy();
  sqlx::query("INSERT INTO tasks(task_id,owner_id,contract_version,state,revision,plan_version,facts_version,plan,creation_digest,checkpoint,completed_nodes) SELECT $1,$2,contract_version,state,revision,plan_version,facts_version,jsonb_set(plan,'{task_id}',to_jsonb($1::text)),creation_digest,checkpoint,completed_nodes FROM tasks WHERE task_id=$3")
   .bind(existing.as_str()).bind(who).bind(base.task.task_id.as_str()).execute(&db.sql).await.unwrap();
  assert!(db.repository.uncreated_chat_request_for_backend(&owner,&existing,false).await.is_err());
 }
 let current=db.repository.chat_request_id_for_backend(&uuid::Uuid::new_v4().to_string(),1).unwrap();
 assert!(db.repository.uncreated_chat_request_for_backend(&owner,&current,false).await.is_err());
 assert_eq!(db.repository.uncreated_chat_request_for_backend(&owner,&current,true).await.unwrap()["proof_source"],"COMPLETED_SERVER_TURN");
 let clone=db.repository.clone();assert_eq!(clone.chat_request_epoch_for_backend(),db.repository.chat_request_epoch_for_backend());
 let mut plan=base.task.plan.clone();plan.task_id=current.clone();
 assert_eq!(clone.create_task(&owner,plan,7,&|| Ok(())).await.err(),Some(StorageError::IdConflict));
 assert_eq!(db.service().create(&session(),&current,args(),7).await.err(),Some(StorageError::IdConflict));
 let live=db.repository.chat_request_id_for_backend(&uuid::Uuid::new_v4().to_string(),1).unwrap();
 db.service().create(&session(),&live,args(),7).await.unwrap();
 assert!(db.repository.uncreated_chat_request_for_backend(&owner,&live,true).await.is_err());
 // An independently constructed repository is NOT a cross-process fence.
 // It also may not issue proof for a v2 epoch whose journal it does not own.
 let url=std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
 let restarted=PgRepository::connect_existing(&url,&db.namespace).await.unwrap();
 assert!(restarted.uncreated_chat_request_for_backend(&owner,&current,true).await.is_err());
 let arbitrary=TaskId::parse("unrelated_request").unwrap();
 assert!(db.repository.uncreated_chat_request_for_backend(&owner,&arbitrary,true).await.is_err());
 assert!(db.repository.uncreated_chat_request_for_backend(&owner,&base.task.task_id,true).await.is_err());
 db.finish().await;
}
#[tokio::test]
#[ignore="new owned PostgreSQL; proof waits for creator transaction commit"]
async fn orphan_chat_proof_serializes_with_generic_creator() {
 let db=Db::new().await;let owner=safeflow_contracts::UserId::parse("user_001").unwrap();
 let base=db.confirmed().await;
 let id=db.repository.chat_request_id_for_backend(&uuid::Uuid::new_v4().to_string(),1).unwrap();
 let mut tx=db.sql.begin().await.unwrap();db.repository.check_chat_creation(&mut tx,&owner,&id).await.unwrap();
 let clone=db.repository.clone();let query=clone.uncreated_chat_request_for_backend(&owner,&id,true);tokio::pin!(query);
 assert!(tokio::time::timeout(Duration::from_millis(80),&mut query).await.is_err());
 sqlx::query("INSERT INTO tasks(task_id,owner_id,contract_version,state,revision,plan_version,facts_version,plan,creation_digest,checkpoint,completed_nodes) SELECT $1,owner_id,contract_version,state,revision,plan_version,facts_version,jsonb_set(plan,'{task_id}',to_jsonb($1::text)),creation_digest,checkpoint,completed_nodes FROM tasks WHERE task_id=$2")
 .bind(id.as_str()).bind(base.task.task_id.as_str()).execute(&mut *tx).await.unwrap();tx.commit().await.unwrap();
 assert!(query.await.is_err());db.finish().await;
}
