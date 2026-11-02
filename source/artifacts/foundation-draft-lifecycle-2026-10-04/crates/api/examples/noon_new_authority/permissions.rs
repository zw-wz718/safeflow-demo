use super::*;

const READS:&[&str]=&["tasks","operations","public_execution_requests","payment_preparations","payment_funds_execution_v1","server_confirmations","execution_permits","mock_draft_lifecycle_plans","transfer_drafts_v1","transfer_business_orders_v1","transfer_fund_orders_v1","transfer_accounts_v1","transfer_ledger_v1","audit_events"];
const COMMON:&[&str]=&["payment_background_identity_v1()","payment_auto_scan_v1(uuid,text,integer)"];
const PUBLIC:&[&str]=&["billing_prepare_integrity_v2()","billing_audit_check_v1()","billing_audit_association_v1()","subscription_a2_association_v1()","public_card_final_expected_digest_v1(uuid,text)","payment_completion_rows_v1(text,jsonb)"];
fn functions(worker:bool)->Vec<String>{
 let mut result=COMMON.iter().map(|s|format!("{NS}.{s}")).collect::<Vec<_>>();
 for s in if worker {vec!["payment_background_worker_v1(text)","payment_completion_begin_v1(text,bigint)","payment_completion_finish_v1(text,bigint)","payment_completion_recover_v1(text)","payment_completion_outgoing_v1(text)","payment_completion_final_guard_v1(text,bigint)"]}else{vec!["payment_background_decide_v1(text,text,text)"]}{result.push(format!("{NS}.{s}"));}
 result
}

// This contract is for the fixed noon v42 deployment, not arbitrary PG owners.
// New-database provisioning runs migrations 1..42 as the fixed resource owner.
// Migration 38 preserves two INVOKER trigger owners and the digest table owner;
// it does not assign a different SECURITY DEFINER owner. Maintenance 43 keeps
// replaced owners and explicitly assigns its four new functions to OWNER.
// Never learn a trusted definer owner or policy expression from the target.
async fn trusted_application(tx:&mut Transaction<'_,Postgres>)->Result<(),Error>{
  let predecessor_roles=super::runtime_creator::validated_roles(tx).await?;
 let safe_owner:bool=sqlx::query_scalar(r#"
 SELECT EXISTS(SELECT 1 FROM pg_roles o WHERE o.rolname=$1
  AND NOT o.rolcanlogin AND NOT o.rolsuper AND NOT o.rolcreatedb
  AND NOT o.rolcreaterole AND NOT o.rolreplication AND NOT o.rolbypassrls
  AND o.rolconfig IS NULL AND o.rolvaliduntil IS NULL
  AND NOT EXISTS(SELECT 1 FROM pg_auth_members m WHERE m.roleid=o.oid)
  AND NOT EXISTS(SELECT 1 FROM pg_auth_members m JOIN pg_roles child ON child.oid=m.roleid
   WHERE m.member=o.oid AND (m.set_option OR m.inherit_option OR NOT m.admin_option
    OR pg_get_userbyid(m.grantor)<>'postgres'
    OR child.rolsuper OR child.rolcreatedb OR child.rolcreaterole
    OR child.rolreplication OR child.rolbypassrls OR NOT child.rolcanlogin
    OR child.rolconfig IS NOT NULL OR child.rolvaliduntil IS NOT NULL
     OR (NOT EXISTS(SELECT 1 FROM sf_unified_noon_acceptance.payment_background_api_logins_v1 a
      WHERE a.role_name=child.rolname AND a.active) AND NOT(child.oid::bigint=ANY($3)))
    OR EXISTS(SELECT 1 FROM pg_auth_members x WHERE x.member=child.oid))))
 AND NOT EXISTS(SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
  WHERE n.nspname=$2 AND p.prosecdef
   AND (pg_get_userbyid(p.proowner)<>$1 OR p.prokind<>'f' OR p.proleakproof))
  "#).bind(OWNER).bind(NS).bind(&predecessor_roles).fetch_one(&mut **tx).await?;
 require(safe_owner)?;
 let reads=READS.iter().map(|s|s.to_string()).collect::<Vec<_>>();
 // Migration 36 creates exactly one permissive ALL/PUBLIC policy on each
 // protected relation, with the same trusted helper in USING and WITH CHECK.
 // pg_catalog-only search_path makes function qualification deterministic.
 let valid_rls:bool=sqlx::query_scalar(r#"
 WITH protected AS (SELECT c.* FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
  WHERE n.nspname=$1 AND c.relname=ANY($2)),
 policies AS (SELECT p.*,c.relname FROM pg_policy p JOIN pg_class c ON c.oid=p.polrelid
  JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1)
 SELECT (SELECT count(*) FROM protected)=$3
  AND NOT EXISTS(SELECT 1 FROM protected c WHERE c.relkind<>'r'
   OR pg_get_userbyid(c.relowner)<>$4 OR NOT c.relrowsecurity OR c.relforcerowsecurity
   OR (SELECT count(*) FROM policies p WHERE p.polrelid=c.oid)<>1)
  AND (SELECT count(*) FROM policies)=$3
  AND NOT EXISTS(SELECT 1 FROM policies p WHERE NOT(p.relname=ANY($2))
   OR p.polname<>'completion_scope_v1' OR p.polcmd<>'*' OR NOT p.polpermissive
   OR p.polroles<>ARRAY[0::oid]
   OR pg_get_expr(p.polqual,p.polrelid,false) IS DISTINCT FROM
    format('%I.payment_completion_rows_v1(%L::text, to_jsonb(%I.*))',$1,p.relname,p.relname)
   OR pg_get_expr(p.polwithcheck,p.polrelid,false) IS DISTINCT FROM
    format('%I.payment_completion_rows_v1(%L::text, to_jsonb(%I.*))',$1,p.relname,p.relname))
 "#).bind(NS).bind(&reads).bind(READS.len() as i64).bind(OWNER).fetch_one(&mut **tx).await?;
 require(valid_rls)
}

pub(super) async fn lock_rls(tx:&mut Transaction<'_,Postgres>)->Result<(),Error>{
 for table in READS{
  sqlx::query(&format!("LOCK TABLE {NS}.{table} IN SHARE ROW EXCLUSIVE MODE"))
   .execute(&mut **tx).await?;
 }
 Ok(())
}
pub(super) async fn catalog(tx:&mut Transaction<'_,Postgres>)->Result<String,Error>{
 Ok(sqlx::query_scalar(r#"
 WITH n AS (SELECT oid,nspname,nspowner,(SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY grantor,grantee,privilege_type),'[]') FROM aclexplode(coalesce(nspacl,acldefault('n',nspowner))) a) AS acl FROM pg_namespace),
 c AS (SELECT c.oid,c.relnamespace,c.relname,c.relowner,c.relkind,c.reloptions,c.relrowsecurity,c.relforcerowsecurity,
 (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY grantor,grantee,privilege_type),'[]') FROM aclexplode(coalesce(c.relacl,acldefault(CASE WHEN c.relkind='S' THEN 's'::"char" ELSE 'r'::"char" END,c.relowner))) a) AS acl,
 CASE WHEN c.relkind IN('v','m') THEN pg_get_viewdef(c.oid,false) END AS view FROM pg_class c),
 a AS (SELECT attrelid,attnum,attname,atttypid,atttypmod,attnotnull,attisdropped,
 (SELECT coalesce(jsonb_agg(to_jsonb(x) ORDER BY grantor,grantee,privilege_type),'[]') FROM aclexplode(attacl) x) AS acl FROM pg_attribute WHERE attnum>0),
 p AS (SELECT oid,pronamespace,proowner,pg_get_functiondef(oid) AS definition,
 (SELECT coalesce(jsonb_agg(to_jsonb(x) ORDER BY grantor,grantee,privilege_type),'[]') FROM aclexplode(coalesce(proacl,acldefault('f',proowner))) x) AS acl FROM pg_proc WHERE prokind IN('f','p')),
 r AS (SELECT oid,ev_class,ev_enabled,pg_get_ruledef(oid,false) AS definition FROM pg_rewrite),
 t AS (SELECT oid,tgrelid,tgfoid,tgenabled,pg_get_triggerdef(oid,false) AS definition FROM pg_trigger),
 policy AS (SELECT oid,polrelid,polname,polcmd,polpermissive,polroles,
 pg_get_expr(polqual,polrelid,false) AS using_expression,
 pg_get_expr(polwithcheck,polrelid,false) AS check_expression FROM pg_policy)
 SELECT encode(sha256(convert_to(jsonb_build_object('n',(SELECT jsonb_agg(to_jsonb(n) ORDER BY oid) FROM n),'c',(SELECT jsonb_agg(to_jsonb(c) ORDER BY oid) FROM c),'a',(SELECT jsonb_agg(to_jsonb(a) ORDER BY attrelid,attnum) FROM a),'p',(SELECT jsonb_agg(to_jsonb(p) ORDER BY oid) FROM p),'r',(SELECT jsonb_agg(to_jsonb(r) ORDER BY oid) FROM r),'t',(SELECT jsonb_agg(to_jsonb(t) ORDER BY oid) FROM t),'policy',(SELECT jsonb_agg(to_jsonb(policy) ORDER BY oid) FROM policy),'resource_owner',(SELECT to_jsonb(o) FROM pg_roles o WHERE rolname='sf_noon_owner_20261029'),'memberships',(SELECT jsonb_agg(to_jsonb(m) ORDER BY roleid,member,grantor) FROM pg_auth_members m),'defaults',(SELECT jsonb_agg(to_jsonb(d) ORDER BY oid) FROM pg_default_acl d),'database',(SELECT to_jsonb(d)-'datfrozenxid'-'datminmxid' FROM pg_database d WHERE datname=current_database()))::text,'UTF8')),'hex')
 "#).fetch_one(&mut **tx).await?)
}
pub(super) async fn validate(tx:&mut Transaction<'_,Postgres>,registered:bool)->Result<(),Error>{
 sqlx::query("SET LOCAL search_path=pg_catalog").execute(&mut **tx).await?;
 let actual:Value=sqlx::query_scalar(include_str!("../noon_pg16_native.sql")).fetch_one(&mut **tx).await?;
 let expected=super::native_profiles::native_expected(&actual)?;
 trusted_application(tx).await?;
 let native=expected["relations"].as_array().ok_or("native list")?.iter().map(|r|r["oid"].as_i64().ok_or("native oid")).collect::<Result<Vec<_>,_>>()?;
 for (role,worker) in [(PRODUCER,false),(WORKER,true)]{
  let oid:Option<i64>=sqlx::query_scalar("SELECT oid::bigint FROM pg_roles WHERE rolname=$1").bind(role).fetch_optional(&mut **tx).await?;
  require(oid.is_some()==registered)?;
  let allowed_reads=if worker{READS.iter().map(|s|s.to_string()).collect::<Vec<_>>()}else{vec![]};
  let mut allowed_functions=functions(worker);allowed_functions.extend(PUBLIC.iter().map(|s|format!("{NS}.{s}")));
  let bad:bool=sqlx::query_scalar(r#"
  WITH s AS (SELECT $1::bigint::oid AS oid), relations AS (SELECT c.*,n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN('r','p','v','m','f')),
  cols AS (SELECT c.oid,c.nspname,c.relname,a.attname,
   coalesce(has_column_privilege(s.oid,c.oid,a.attnum,'SELECT'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(c.relacl,acldefault('r',c.relowner))) x WHERE x.grantee=0 AND x.privilege_type='SELECT') OR EXISTS(SELECT 1 FROM aclexplode(a.attacl) x WHERE x.grantee=0 AND x.privilege_type='SELECT') AS readable,
   coalesce(has_column_privilege(s.oid,c.oid,a.attnum,'UPDATE'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(c.relacl,acldefault('r',c.relowner))) x WHERE x.grantee=0 AND x.privilege_type='UPDATE') OR EXISTS(SELECT 1 FROM aclexplode(a.attacl) x WHERE x.grantee=0 AND x.privilege_type='UPDATE') AS updatable,
   coalesce(has_column_privilege(s.oid,c.oid,a.attnum,'INSERT'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(c.relacl,acldefault('r',c.relowner))) x WHERE x.grantee=0 AND x.privilege_type='INSERT') OR EXISTS(SELECT 1 FROM aclexplode(a.attacl) x WHERE x.grantee=0 AND x.privilege_type='INSERT') AS insertable,
   coalesce(has_column_privilege(s.oid,c.oid,a.attnum,'REFERENCES'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(c.relacl,acldefault('r',c.relowner))) x WHERE x.grantee=0 AND x.privilege_type='REFERENCES') OR EXISTS(SELECT 1 FROM aclexplode(a.attacl) x WHERE x.grantee=0 AND x.privilege_type='REFERENCES') AS referenceable
   FROM relations c JOIN pg_attribute a ON a.attrelid=c.oid CROSS JOIN s WHERE a.attnum>0 AND NOT a.attisdropped)
  SELECT EXISTS(SELECT 1 FROM cols WHERE (readable AND NOT(oid::bigint=ANY($2)) AND NOT(nspname=$3 AND relname=ANY($4))) OR referenceable
    OR (updatable AND NOT(oid=12104 AND oid::bigint=ANY($2)) AND NOT($5 AND nspname=$3 AND ((relname='transfer_accounts_v1' AND attname='booked_cents') OR (relname='transfer_fund_orders_v1' AND attname='state') OR (relname='transfer_business_orders_v1' AND attname IN('latest_receipt','latest_recorded_at')))))
    OR (insertable AND NOT($5 AND nspname=$3 AND relname='transfer_ledger_v1' AND attname IN('business_owner_id','operation_id','account_id','direction','amount_cents','completed_at'))))
  OR EXISTS(SELECT 1 FROM relations c CROSS JOIN s WHERE coalesce(has_table_privilege(s.oid,c.oid,'DELETE,TRUNCATE,TRIGGER'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(c.relacl,acldefault('r',c.relowner))) a WHERE a.grantee=0 AND a.privilege_type IN('DELETE','TRUNCATE','TRIGGER')))
  OR EXISTS(SELECT 1 FROM pg_class c CROSS JOIN s WHERE c.relkind='S' AND (coalesce(has_sequence_privilege(s.oid,c.oid,'SELECT,USAGE,UPDATE'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(c.relacl,acldefault('s',c.relowner))) a WHERE a.grantee=0)))
  OR EXISTS(SELECT 1 FROM pg_namespace n CROSS JOIN s WHERE coalesce(has_schema_privilege(s.oid,n.oid,'CREATE'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(n.nspacl,acldefault('n',n.nspowner))) a WHERE a.grantee=0 AND a.privilege_type='CREATE'))
  OR EXISTS(SELECT 1 FROM pg_proc p CROSS JOIN s WHERE p.prosecdef AND NOT(p.oid::regprocedure::text=ANY($6)) AND (coalesce(has_function_privilege(s.oid,p.oid,'EXECUTE'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(p.proacl,acldefault('f',p.proowner))) a WHERE a.grantee=0 AND a.privilege_type='EXECUTE')))
  "#).bind(oid).bind(&native).bind(NS).bind(&allowed_reads).bind(worker).bind(&allowed_functions).fetch_one(&mut **tx).await?;
  require(!bad)?;
  if registered {exact_grants(tx,role,worker).await?;}
 }
 sqlx::raw_sql("SET LOCAL search_path=sf_unified_noon_acceptance,pg_catalog,pg_temp").execute(&mut **tx).await?;
 Ok(())
}

async fn exact_grants(tx:&mut Transaction<'_,Postgres>,role:&str,worker:bool)->Result<(),Error>{
 let grants:Value=sqlx::query_scalar(r#"
 WITH s AS (SELECT oid FROM pg_roles WHERE rolname=$1), entries AS (
 SELECT 'database'::text AS kind,d.datname::text AS object,''::text AS col,a.* FROM pg_database d CROSS JOIN LATERAL aclexplode(d.datacl) a
 UNION ALL SELECT 'schema',n.nspname::text,'',a.* FROM pg_namespace n CROSS JOIN LATERAL aclexplode(n.nspacl) a
 UNION ALL SELECT 'relation',n.nspname||'.'||c.relname,'',a.* FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace CROSS JOIN LATERAL aclexplode(c.relacl) a
 UNION ALL SELECT 'column',n.nspname||'.'||c.relname,t.attname::text,a.* FROM pg_attribute t JOIN pg_class c ON c.oid=t.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace CROSS JOIN LATERAL aclexplode(t.attacl) a
 UNION ALL SELECT 'function',p.oid::regprocedure::text,'',a.* FROM pg_proc p CROSS JOIN LATERAL aclexplode(p.proacl) a
 UNION ALL SELECT 'type',n.nspname||'.'||t.typname,'',a.* FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace CROSS JOIN LATERAL aclexplode(t.typacl) a
 UNION ALL SELECT 'language',l.lanname::text,'',a.* FROM pg_language l CROSS JOIN LATERAL aclexplode(l.lanacl) a
 UNION ALL SELECT 'foreign_server',f.srvname::text,'',a.* FROM pg_foreign_server f CROSS JOIN LATERAL aclexplode(f.srvacl) a
 UNION ALL SELECT 'foreign_wrapper',f.fdwname::text,'',a.* FROM pg_foreign_data_wrapper f CROSS JOIN LATERAL aclexplode(f.fdwacl) a
 UNION ALL SELECT 'large_object',l.oid::text,'',a.* FROM pg_largeobject_metadata l CROSS JOIN LATERAL aclexplode(l.lomacl) a
 UNION ALL SELECT 'parameter',p.parname::text,'',a.* FROM pg_parameter_acl p CROSS JOIN LATERAL aclexplode(p.paracl) a)
 SELECT coalesce(jsonb_agg(jsonb_build_array(kind,object,col,pg_get_userbyid(grantor),privilege_type,is_grantable) ORDER BY kind,object,col,privilege_type),'[]') FROM entries e JOIN s ON s.oid=e.grantee
 "#).bind(role).fetch_one(&mut **tx).await?;
 let mut expected=vec![json!(["database",DB,"",OWNER,"CONNECT",false]),json!(["schema",NS,"",OWNER,"USAGE",false])];
 // Function owners are the original protected nonlogin resource owners. The
 // exact grantor is bound in the immutable post-catalog digest; never grantable.
 for signature in functions(worker){
  let owner:String=sqlx::query_scalar("SELECT pg_get_userbyid(proowner)::text FROM pg_proc WHERE oid=$1::regprocedure").bind(&signature).fetch_one(&mut **tx).await?;
  expected.push(json!(["function",signature,"",owner,"EXECUTE",false]));
 }
 if worker {
  for table in READS{expected.push(json!(["relation",format!("{NS}.{table}"),"",OWNER,"SELECT",false]));}
  for (table,columns,privilege) in [("transfer_accounts_v1",vec!["booked_cents"],"UPDATE"),("transfer_fund_orders_v1",vec!["state"],"UPDATE"),("transfer_business_orders_v1",vec!["latest_receipt","latest_recorded_at"],"UPDATE"),("transfer_ledger_v1",vec!["business_owner_id","operation_id","account_id","direction","amount_cents","completed_at"],"INSERT")]{
   for col in columns {expected.push(json!(["column",format!("{NS}.{table}"),col,OWNER,privilege,false]));}
  }
 }
 expected.sort_by(|a,b|{for index in [0,1,2,4] {let order=a[index].as_str().cmp(&b[index].as_str());if !order.is_eq(){return order;}}std::cmp::Ordering::Equal});
 require(grants==Value::Array(expected))
}
