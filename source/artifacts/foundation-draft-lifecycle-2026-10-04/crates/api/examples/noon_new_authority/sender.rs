//! Read-only extraction from frozen sender 0054312a. Never registers or grants.
use super::*;
const ROLE:&str="sf_noon_audit_sender_20261030";
fn fingerprint(v:&Value)->Result<String,Error>{super::digest(v)}
async fn scope(tx: &mut Transaction<'_, Postgres>) -> Result<Value, Error> {
    let identity: Value = sqlx::query_scalar("SELECT jsonb_build_object('database',d.datname,'database_oid',d.oid::bigint,'database_owner',pg_get_userbyid(d.datdba),'namespace',n.nspname,'namespace_oid',n.oid::bigint,'namespace_owner',pg_get_userbyid(n.nspowner),'outbox_oid',c.oid::bigint,'outbox_owner',pg_get_userbyid(c.relowner),'maintenance_user',current_user,'maintenance_session',session_user,'maintenance_superuser',r.rolsuper,'database_public_acl',coalesce(d.datacl::text,''),'schema_public_acl',coalesce(n.nspacl::text,'')) FROM pg_database d JOIN pg_namespace n ON n.nspname=$1 JOIN pg_class c ON c.relnamespace=n.oid AND c.relname='public_audit_outbox' AND c.relkind='r' JOIN pg_roles r ON r.rolname=current_user WHERE d.datname=current_database()")
        .bind(NS).fetch_one(&mut **tx).await?;
    require(identity["database"]==DB && identity["database_owner"]==OWNER && identity["namespace_owner"]==OWNER && identity["outbox_owner"]==OWNER)?;
    require(identity["maintenance_user"]=="postgres" && identity["maintenance_session"]=="postgres" && identity["maintenance_superuser"]==true)?;
    let migrations: Value = sqlx::query_scalar("SELECT coalesce(jsonb_agg(jsonb_build_object('version',version,'success',success,'checksum',encode(checksum,'hex')) ORDER BY version),'[]'::jsonb) FROM sf_unified_noon_acceptance._sqlx_migrations").fetch_one(&mut **tx).await?;
    let ms = migrations.as_array().ok_or("migration list")?;
    require(ms.len()==42 && ms.iter().enumerate().all(|(i,m)|m["version"]==json!(i+1) && m["success"]==true))?;
    for (version, checksum) in [(40,"aeddbc3fdc662c5804b0bd14110ea41efa4ff91f62a6ddea14036713c2bb447a59d0b0f900549ea6d1930299ef13f56f"),(41,"f1e1c5c227911aa531e8976b9ba7e735c496078f015b65da7ce21016e93408a3a3961e20b03b19f48dc3673ee4d9ba21"),(42,"a2df799ddb4fa68d4b1aad849e28b43382f5dca15dba7abaf645b0dfc6268fa9f4ef88d519282b528346963c4abbbea0")] {
        require(ms[(version-1) as usize]["checksum"]==checksum)?;
    }
    let columns: Value = sqlx::query_scalar("SELECT jsonb_agg(jsonb_build_array(a.attname,format_type(a.atttypid,a.atttypmod),a.attnotnull) ORDER BY a.attnum) FROM pg_attribute a WHERE a.attrelid='sf_unified_noon_acceptance.public_audit_outbox'::regclass AND a.attnum>0 AND NOT a.attisdropped").fetch_one(&mut **tx).await?;
    require(columns==json!([["event_id","text",true],["sequence","bigint",true],["envelope","jsonb",true],["enqueued_at","timestamp with time zone",true],["historical","boolean",true],["receiver","text",false],["receipt_ref","text",false],["acknowledged_at","timestamp with time zone",false]]))?;
    let protection: Value = sqlx::query_scalar("SELECT jsonb_agg(jsonb_build_object('trigger',t.tgname,'trigger_oid',t.oid::bigint,'function_oid',t.tgfoid::bigint,'enabled',t.tgenabled,'definition',pg_get_triggerdef(t.oid),'function',pg_get_functiondef(t.tgfoid)) ORDER BY t.tgname) FROM pg_trigger t WHERE t.tgrelid='sf_unified_noon_acceptance.public_audit_outbox'::regclass AND NOT t.tgisinternal").fetch_one(&mut **tx).await?;
    let immutable=protection.as_array().and_then(|a|a.iter().find(|p|p["trigger"]=="public_audit_outbox_immutable")).ok_or("immutable trigger")?;
    require(immutable["enabled"]=="O"
        && format!("{:x}",Sha256::digest(immutable["definition"].as_str().ok_or("trigger")?.as_bytes()))=="8a6b3bf984db2b9ccd81bb69eca0fb2969f2601dbd297e9c214ded93aafd257b"
        && format!("{:x}",Sha256::digest(immutable["function"].as_str().ok_or("function")?.as_bytes()))=="e6fd49f19bbab455f972b92407d4a639b91a7b2ecd4824962574521f6124f3df")?;
    let immutable_identity: Value=sqlx::query_scalar("SELECT jsonb_build_object('namespace',n.nspname,'signature',p.oid::regprocedure::text,'owner',pg_get_userbyid(p.proowner),'definer',p.prosecdef,'leakproof',p.proleakproof,'volatility',p.provolatile,'parallel',p.proparallel,'config',p.proconfig,'kind',p.prokind,'return',p.prorettype::regtype::text,'trigger_type',t.tgtype,'internal',t.tgisinternal) FROM pg_trigger t JOIN pg_proc p ON p.oid=t.tgfoid JOIN pg_namespace n ON n.oid=p.pronamespace WHERE t.tgrelid='sf_unified_noon_acceptance.public_audit_outbox'::regclass AND t.tgname='public_audit_outbox_immutable'").fetch_one(&mut **tx).await?;
    require(immutable_identity==json!({"namespace":NS,"signature":"sf_unified_noon_acceptance.protect_public_audit_outbox()","owner":OWNER,"definer":false,"leakproof":false,"volatility":"v","parallel":"u","config":null,"kind":"f","return":"trigger","trigger_type":27,"internal":false}))?;
    // v42 inherits four trigger-only checks and two STABLE policy helpers via
    // PUBLIC. They grant no table privilege or business write. Pin their entire
    // definitions; never widen PUBLIC or grant any function to the sender.
    let definers: Value=sqlx::query_scalar("SELECT coalesce(jsonb_agg(jsonb_build_object('signature',p.oid::regprocedure::text,'definition',pg_get_functiondef(p.oid)) ORDER BY p.oid::regprocedure::text),'[]'::jsonb) FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname=$1 AND p.prosecdef AND EXISTS(SELECT 1 FROM aclexplode(coalesce(p.proacl,acldefault('f',p.proowner))) a WHERE a.grantee=0 AND a.privilege_type='EXECUTE')").bind(NS).fetch_one(&mut **tx).await?;
    let pinned=[
        ("sf_unified_noon_acceptance.billing_prepare_integrity_v2()","c047310b51e1591517f03e739d78a9ef57a3c4348fc28f15d0cc72783638181c"),
        ("sf_unified_noon_acceptance.billing_audit_check_v1()","3851add914e892867bce383d736f7058e6567e181f5af461bbca7cb0f0c0acf7"),
        ("sf_unified_noon_acceptance.billing_audit_association_v1()","6109b28de42f344e5d4682d82a6cc27508f5bbc6e4a207507d011b90567f7d8a"),
        ("sf_unified_noon_acceptance.subscription_a2_association_v1()","28b4db42ede41d54a279d02bef2beef6b158884bbf666ccbcb4f972c717092d6"),
        ("sf_unified_noon_acceptance.public_card_final_expected_digest_v1(uuid,text)","fe0752e60bd67394dcaceea57b04a76656e560dd3988838c076bcd5c911adc6c"),
        ("sf_unified_noon_acceptance.payment_completion_rows_v1(text,jsonb)","1b39bd59df9ae155fc3607445aefb9542740d5509a956373b1de900e0c956583"),
    ];
    let functions=definers.as_array().ok_or("definer list")?;require(functions.len()==pinned.len())?;
    for (signature,digest) in pinned {
        let f=functions.iter().find(|f|f["signature"]==signature).ok_or("definer signature")?;
        require(format!("{:x}",Sha256::digest(f["definition"].as_str().ok_or("definer body")?.as_bytes()))==digest)?;
    }
    let bad_owner: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname=$1 AND p.prosecdef AND EXISTS(SELECT 1 FROM aclexplode(coalesce(p.proacl,acldefault('f',p.proowner))) a WHERE a.grantee=0 AND a.privilege_type='EXECUTE') AND (pg_get_userbyid(p.proowner)<>$2 OR p.prokind<>'f' OR p.proleakproof))").bind(NS).bind(OWNER).fetch_one(&mut **tx).await?;
    require(!bad_owner)?;
    // Validate PUBLIC's prospective authority before any role exists too.
    // After creation, effective checks include the real role and all columns.
    validate_surface(tx).await?;
    // Compact CAS binding for the wider authority catalog. Exclude ONLY this
    // exact role's ACL entries: verify_direct_grants closes those independently.
    let catalog_sha256: String=sqlx::query_scalar(r#"
WITH sender AS (SELECT coalesce((SELECT oid FROM pg_roles WHERE rolname=$1),4294967295::oid) AS oid),
ns AS (SELECT n.oid,n.nspname,n.nspowner,(SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.grantor,a.grantee,a.privilege_type),'[]') FROM aclexplode(coalesce(n.nspacl,acldefault('n',n.nspowner))) a,sender WHERE a.grantee<>sender.oid) AS acl FROM pg_namespace n),
rels AS (SELECT c.oid,c.relnamespace,c.relname,c.relkind,c.relowner,c.reloptions,CASE WHEN c.relkind IN('v','m') THEN pg_get_viewdef(c.oid,false) END AS view,(SELECT coalesce(jsonb_agg(jsonb_build_object('oid',r.oid,'enabled',r.ev_enabled,'definition',pg_get_ruledef(r.oid,false)) ORDER BY r.oid),'[]') FROM pg_rewrite r WHERE r.ev_class=c.oid) AS rules,(SELECT coalesce(jsonb_agg(jsonb_build_object('oid',t.oid,'function',t.tgfoid,'enabled',t.tgenabled,'definition',pg_get_triggerdef(t.oid,false)) ORDER BY t.oid),'[]') FROM pg_trigger t WHERE t.tgrelid=c.oid) AS triggers,(SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.grantor,a.grantee,a.privilege_type),'[]') FROM aclexplode(coalesce(c.relacl,acldefault(CASE WHEN c.relkind='S' THEN 's'::"char" ELSE 'r'::"char" END,c.relowner))) a,sender WHERE a.grantee<>sender.oid) AS acl FROM pg_class c JOIN ns ON ns.oid=c.relnamespace),
cols AS (SELECT a.attrelid,a.attnum,a.attname,(SELECT coalesce(jsonb_agg(to_jsonb(x) ORDER BY x.grantor,x.grantee,x.privilege_type),'[]') FROM aclexplode(a.attacl) x,sender WHERE x.grantee<>sender.oid) AS acl FROM pg_attribute a JOIN rels ON rels.oid=a.attrelid WHERE a.attnum>0 AND NOT a.attisdropped),
funcs AS (SELECT p.oid,p.pronamespace,p.proowner,pg_get_functiondef(p.oid) AS definition,(SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.grantor,a.grantee,a.privilege_type),'[]') FROM aclexplode(coalesce(p.proacl,acldefault('f',p.proowner))) a,sender WHERE a.grantee<>sender.oid) AS acl FROM pg_proc p WHERE p.prosecdef AND p.prokind IN('f','p'))
SELECT encode(sha256(convert_to(jsonb_build_object('schemas',(SELECT jsonb_agg(to_jsonb(ns) ORDER BY oid) FROM ns),'relations',(SELECT jsonb_agg(to_jsonb(rels) ORDER BY oid) FROM rels),'columns',(SELECT jsonb_agg(to_jsonb(cols) ORDER BY attrelid,attnum) FROM cols),'definers',(SELECT jsonb_agg(to_jsonb(funcs) ORDER BY oid) FROM funcs),'defaults',(SELECT jsonb_agg(to_jsonb(d) ORDER BY oid) FROM pg_default_acl d),'owner',(SELECT to_jsonb(r)-'rolpassword' FROM pg_roles r WHERE rolname=$2))::text,'UTF8')),'hex')
"#).bind(ROLE).bind(OWNER).fetch_one(&mut **tx).await?;
    // Own sender ACL entries change only on this tool's first apply, so exclude
    // that exact grantee from identity's ACL fingerprint; all others remain CAS.
    let mut identity=identity;
    let acls: Value=sqlx::query_scalar("SELECT jsonb_build_object('database',(SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.grantee,a.privilege_type),'[]'::jsonb) FROM pg_database d CROSS JOIN LATERAL aclexplode(coalesce(d.datacl,acldefault('d',d.datdba))) a WHERE d.datname=current_database() AND a.grantee<>coalesce((SELECT oid FROM pg_roles WHERE rolname=$1),4294967295::oid)),'schema',(SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.grantee,a.privilege_type),'[]'::jsonb) FROM pg_namespace n CROSS JOIN LATERAL aclexplode(coalesce(n.nspacl,acldefault('n',n.nspowner))) a WHERE n.nspname=$2 AND a.grantee<>coalesce((SELECT oid FROM pg_roles WHERE rolname=$1),4294967295::oid)))").bind(ROLE).bind(NS).fetch_one(&mut **tx).await?;
    identity.as_object_mut().ok_or("identity")?.remove("database_public_acl");
    identity.as_object_mut().ok_or("identity")?.remove("schema_public_acl");
    Ok(json!({"identity":identity,"migrations":migrations,"columns":columns,"protection":protection,"immutable_identity":immutable_identity,"other_acls":acls,"inherited_public_readonly_or_trigger_definers":definers,"authority_catalog_sha256":catalog_sha256}))
}

// Neither a namespace name nor a live database's first plan is a trusted origin.
// This build-time baseline came from a separate pristine PG16.15 template0 DB.
// Pin native object identities, columns/ACLs, complete view/rule/trigger sets and
// native function metadata (including the GUC callees). Unknown objects receive
// no exception. Drift fails check/plan as well as old-plan apply.
async fn native_relations(tx: &mut Transaction<'_, Postgres>) -> Result<Vec<i64>, Error> {
    let actual: Value=sqlx::query_scalar(include_str!("../noon_pg16_native.sql")).fetch_one(&mut **tx).await?;
    let expected=native_profiles::native_expected(&actual)?;
    expected["relations"].as_array().ok_or("native relation list")?.iter()
        .map(|r|r["oid"].as_i64().ok_or_else(||"native relation identity".into())).collect()
}

// PostgreSQL's exact approved pg_settings retains PUBLIC UPDATE and native GUC
// checks. No additional business grants or application helper exceptions.
async fn validate_surface(tx: &mut Transaction<'_, Postgres>) -> Result<(), Error> {
    let native=native_relations(tx).await?;
    let oid: Option<i64>=sqlx::query_scalar("SELECT oid::bigint FROM pg_roles WHERE rolname=$1").bind(ROLE).fetch_optional(&mut **tx).await?;
    let bad: bool=sqlx::query_scalar(r#"
WITH s AS (SELECT $1::bigint::oid AS oid), relations AS (
 SELECT c.*,n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN('r','p','v','m','f')
), effective_columns AS (
 SELECT c.oid,c.relname,c.nspname,a.attname,
 coalesce(has_column_privilege(s.oid,c.oid,a.attnum,'SELECT'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(c.relacl,acldefault('r',c.relowner))) x WHERE x.grantee=0 AND x.privilege_type='SELECT') OR EXISTS(SELECT 1 FROM aclexplode(a.attacl) x WHERE x.grantee=0 AND x.privilege_type='SELECT') AS readable,
 coalesce(has_column_privilege(s.oid,c.oid,a.attnum,'INSERT,REFERENCES'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(c.relacl,acldefault('r',c.relowner))) x WHERE x.grantee=0 AND x.privilege_type IN('INSERT','REFERENCES')) OR EXISTS(SELECT 1 FROM aclexplode(a.attacl) x WHERE x.grantee=0 AND x.privilege_type IN('INSERT','REFERENCES')) AS insert_or_reference,
 coalesce(has_column_privilege(s.oid,c.oid,a.attnum,'UPDATE'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(c.relacl,acldefault('r',c.relowner))) x WHERE x.grantee=0 AND x.privilege_type='UPDATE') OR EXISTS(SELECT 1 FROM aclexplode(a.attacl) x WHERE x.grantee=0 AND x.privilege_type='UPDATE') AS updatable
 FROM relations c JOIN pg_attribute a ON a.attrelid=c.oid CROSS JOIN s WHERE a.attnum>0 AND NOT a.attisdropped
)
SELECT EXISTS(SELECT 1 FROM effective_columns e WHERE
 (e.readable AND NOT(e.oid::bigint=ANY($3)) AND NOT(e.nspname=$2 AND e.relname='public_audit_outbox'))
 OR e.insert_or_reference OR (e.updatable AND NOT(e.oid=12104 AND e.oid::bigint=ANY($3)) AND NOT(e.nspname=$2 AND e.relname='public_audit_outbox' AND e.attname IN('receiver','receipt_ref','acknowledged_at'))))
 OR EXISTS(SELECT 1 FROM relations c CROSS JOIN s WHERE coalesce(has_table_privilege(s.oid,c.oid,'DELETE,TRUNCATE,TRIGGER'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(c.relacl,acldefault('r',c.relowner))) a WHERE a.grantee=0 AND a.privilege_type IN('DELETE','TRUNCATE','TRIGGER')))
 OR EXISTS(SELECT 1 FROM pg_class c CROSS JOIN s WHERE c.relkind='S' AND (coalesce(has_sequence_privilege(s.oid,c.oid,'SELECT,USAGE,UPDATE'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(c.relacl,acldefault('s',c.relowner))) a WHERE a.grantee=0 AND a.privilege_type IN('SELECT','USAGE','UPDATE'))))
 OR EXISTS(SELECT 1 FROM pg_namespace n CROSS JOIN s WHERE coalesce(has_schema_privilege(s.oid,n.oid,'CREATE'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(n.nspacl,acldefault('n',n.nspowner))) a WHERE a.grantee=0 AND a.privilege_type='CREATE'))
 OR EXISTS(SELECT 1 FROM pg_proc p CROSS JOIN s WHERE p.prosecdef AND p.oid::regprocedure::text NOT IN('sf_unified_noon_acceptance.billing_prepare_integrity_v2()','sf_unified_noon_acceptance.billing_audit_check_v1()','sf_unified_noon_acceptance.billing_audit_association_v1()','sf_unified_noon_acceptance.subscription_a2_association_v1()','sf_unified_noon_acceptance.public_card_final_expected_digest_v1(uuid,text)','sf_unified_noon_acceptance.payment_completion_rows_v1(text,jsonb)') AND (coalesce(has_function_privilege(s.oid,p.oid,'EXECUTE'),false) OR EXISTS(SELECT 1 FROM aclexplode(coalesce(p.proacl,acldefault('f',p.proowner))) a WHERE a.grantee=0 AND a.privilege_type='EXECUTE')))
"#).bind(oid).bind(NS).bind(&native).fetch_one(&mut **tx).await?;
    require(!bad)?;
    verify_direct_grants(tx,oid.is_some()).await
}

async fn verify_direct_grants(tx: &mut Transaction<'_, Postgres>, exists: bool) -> Result<(), Error> {
    let grants:Value=sqlx::query_scalar(r#"
WITH s AS (SELECT oid FROM pg_roles WHERE rolname=$1), entries AS (
 SELECT 'database' AS kind,d.datname::text AS object,''::text AS column_name,a.* FROM pg_database d CROSS JOIN LATERAL aclexplode(d.datacl) a
 UNION ALL SELECT 'schema',n.nspname::text,'',a.* FROM pg_namespace n CROSS JOIN LATERAL aclexplode(n.nspacl) a
 UNION ALL SELECT 'relation',n.nspname||'.'||c.relname,'',a.* FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace CROSS JOIN LATERAL aclexplode(c.relacl) a
 UNION ALL SELECT 'column',n.nspname||'.'||c.relname,t.attname::text,a.* FROM pg_attribute t JOIN pg_class c ON c.oid=t.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace CROSS JOIN LATERAL aclexplode(t.attacl) a
 UNION ALL SELECT 'function',p.oid::regprocedure::text,'',a.* FROM pg_proc p CROSS JOIN LATERAL aclexplode(p.proacl) a
 UNION ALL SELECT 'type',n.nspname||'.'||t.typname,'',a.* FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace CROSS JOIN LATERAL aclexplode(t.typacl) a
 UNION ALL SELECT 'language',l.lanname::text,'',a.* FROM pg_language l CROSS JOIN LATERAL aclexplode(l.lanacl) a
 UNION ALL SELECT 'foreign_server',f.srvname::text,'',a.* FROM pg_foreign_server f CROSS JOIN LATERAL aclexplode(f.srvacl) a
 UNION ALL SELECT 'foreign_wrapper',f.fdwname::text,'',a.* FROM pg_foreign_data_wrapper f CROSS JOIN LATERAL aclexplode(f.fdwacl) a
 UNION ALL SELECT 'large_object',l.oid::text,'',a.* FROM pg_largeobject_metadata l CROSS JOIN LATERAL aclexplode(l.lomacl) a
 UNION ALL SELECT 'parameter',p.parname::text,'',a.* FROM pg_parameter_acl p CROSS JOIN LATERAL aclexplode(p.paracl) a
)
SELECT coalesce(jsonb_agg(jsonb_build_array(kind,object,column_name,pg_get_userbyid(grantor),privilege_type,is_grantable) ORDER BY kind,object,column_name,privilege_type),'[]') FROM entries e JOIN s ON s.oid=e.grantee
"#).bind(ROLE).fetch_one(&mut **tx).await?;
    let expected=if exists {json!([
        ["column","sf_unified_noon_acceptance.public_audit_outbox","acknowledged_at",OWNER,"UPDATE",false],
        ["column","sf_unified_noon_acceptance.public_audit_outbox","receipt_ref",OWNER,"UPDATE",false],
        ["column","sf_unified_noon_acceptance.public_audit_outbox","receiver",OWNER,"UPDATE",false],
        ["database",DB,"",OWNER,"CONNECT",false],
        ["relation","sf_unified_noon_acceptance.public_audit_outbox","",OWNER,"SELECT",false],
        ["schema",NS,"",OWNER,"USAGE",false]
    ])}else{json!([])};
    require(grants==expected)
}

async fn role(tx: &mut Transaction<'_, Postgres>) -> Result<Option<Value>, Error> {
    Ok(sqlx::query_scalar("SELECT jsonb_build_object('oid',oid::bigint,'name',rolname,'login',rolcanlogin,'superuser',rolsuper,'inherit',rolinherit,'create_db',rolcreatedb,'create_role',rolcreaterole,'replication',rolreplication,'bypass_rls',rolbypassrls,'connection_limit',rolconnlimit,'valid_until',rolvaliduntil,'config',rolconfig,'comment',shobj_description(oid,'pg_authid')) FROM pg_roles WHERE rolname=$1").bind(ROLE).fetch_optional(&mut **tx).await?)
}

async fn verify_rights(tx: &mut Transaction<'_, Postgres>, expected_marker: &str) -> Result<Value, Error> {
    validate_surface(tx).await?;
    let r=role(tx).await?.ok_or("sender role missing")?;
    for (k,v) in [("login",true),("superuser",false),("inherit",false),("create_db",false),("create_role",false),("replication",false),("bypass_rls",false)] { require(r[k]==v)?; }
    require(r["comment"]==expected_marker && r["connection_limit"]==4 && r["valid_until"].is_null() && r["config"].is_null())?;
    let bad: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_auth_members WHERE member=(SELECT oid FROM pg_roles WHERE rolname=$1) OR roleid=(SELECT oid FROM pg_roles WHERE rolname=$1)) OR EXISTS(SELECT 1 FROM pg_database WHERE datdba=(SELECT oid FROM pg_roles WHERE rolname=$1)) OR EXISTS(SELECT 1 FROM pg_namespace WHERE nspowner=(SELECT oid FROM pg_roles WHERE rolname=$1)) OR EXISTS(SELECT 1 FROM pg_class WHERE relowner=(SELECT oid FROM pg_roles WHERE rolname=$1))").bind(ROLE).fetch_one(&mut **tx).await?;
    require(!bad)?;
    let rights: Value=sqlx::query_scalar("SELECT jsonb_build_object('connect',has_database_privilege($1,current_database(),'CONNECT'),'create',has_database_privilege($1,current_database(),'CREATE'),'temp',has_database_privilege($1,current_database(),'TEMP'),'usage',has_schema_privilege($1,$2,'USAGE'),'schema_create',has_schema_privilege($1,$2,'CREATE'),'outbox_select',has_table_privilege($1,'sf_unified_noon_acceptance.public_audit_outbox','SELECT'),'bad_table',EXISTS(SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$2 AND c.relkind IN ('r','p','v','m','f','S') AND ((c.relname<>'public_audit_outbox' AND has_table_privilege($1,c.oid,'SELECT')) OR has_table_privilege($1,c.oid,'INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER'))),'update_columns',(SELECT jsonb_agg(a.attname ORDER BY a.attname) FROM pg_attribute a WHERE a.attrelid='sf_unified_noon_acceptance.public_audit_outbox'::regclass AND a.attnum>0 AND NOT a.attisdropped AND has_column_privilege($1,a.attrelid,a.attnum,'UPDATE')),'bad_other_column',EXISTS(SELECT 1 FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$2 AND c.relname<>'public_audit_outbox' AND a.attnum>0 AND NOT a.attisdropped AND has_column_privilege($1,a.attrelid,a.attnum,'INSERT,UPDATE,REFERENCES')),'unapproved_definer_execute',EXISTS(SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname=$2 AND p.prosecdef AND p.oid::regprocedure::text NOT IN ('sf_unified_noon_acceptance.billing_prepare_integrity_v2()','sf_unified_noon_acceptance.billing_audit_check_v1()','sf_unified_noon_acceptance.billing_audit_association_v1()','sf_unified_noon_acceptance.subscription_a2_association_v1()','sf_unified_noon_acceptance.public_card_final_expected_digest_v1(uuid,text)','sf_unified_noon_acceptance.payment_completion_rows_v1(text,jsonb)') AND has_function_privilege($1,p.oid,'EXECUTE')))").bind(ROLE).bind(NS).fetch_one(&mut **tx).await?;
    if rights!=json!({"connect":true,"create":false,"temp":false,"usage":true,"schema_create":false,"outbox_select":true,"bad_table":false,"update_columns":["acknowledged_at","receipt_ref","receiver"],"bad_other_column":false,"unapproved_definer_execute":false}) {
        // Only fixed permission booleans and the validated eight-column catalog.
        eprintln!("NOON_OUTBOX_RIGHTS_REJECTED {rights}");
        return Err("fixed sender rights rejected".into());
    }
    // No grant options or unexpected direct grants in this database.
    let bad_grant: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace CROSS JOIN LATERAL aclexplode(c.relacl) a WHERE a.grantee=(SELECT oid FROM pg_roles WHERE rolname=$1) AND (n.nspname<>$2 OR c.relname<>'public_audit_outbox' OR a.privilege_type<>'SELECT' OR a.is_grantable)) OR EXISTS(SELECT 1 FROM pg_attribute att JOIN pg_class c ON c.oid=att.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace CROSS JOIN LATERAL aclexplode(att.attacl) a WHERE a.grantee=(SELECT oid FROM pg_roles WHERE rolname=$1) AND (n.nspname<>$2 OR c.relname<>'public_audit_outbox' OR att.attname NOT IN ('receiver','receipt_ref','acknowledged_at') OR a.privilege_type<>'UPDATE' OR a.is_grantable))").bind(ROLE).bind(NS).fetch_one(&mut **tx).await?;
    require(!bad_grant)?;
    Ok(json!({"role":r,"rights":rights}))
}


pub(super) async fn registered(tx:&mut Transaction<'_,Postgres>,prior:Option<&Value>)->Result<Value,Error>{
    sqlx::raw_sql("SET LOCAL search_path=pg_catalog").execute(&mut **tx).await?;
    let result=if let Some(prior)=prior{
        let marker=prior["inspection"]["role"]["comment"].as_str().ok_or("trusted predecessor sender missing")?;
        let current=verify_rights(tx,marker).await?;
        require(current==prior["inspection"])?;
        prior.clone()
    }else{
        let s=scope(tx).await?;
        let marker=format!("SAFEFLOW_NOON_OUTBOX_LOGIN_V1:{}",fingerprint(&s)?);
        let inspection=verify_rights(tx,&marker).await?;
        json!({"scope":s,"inspection":inspection})
    };
    sqlx::raw_sql("SET LOCAL search_path=sf_unified_noon_acceptance,pg_catalog,pg_temp").execute(&mut **tx).await?;
    Ok(result)
}
