-- Canonical PostgreSQL 16.15 catalog identities. Run with search_path=pg_catalog.
-- Expected digests are built from a separate pristine template0 database, never
-- accepted from the application database by check, plan, or apply.
WITH native AS (
 SELECT c.oid,c.relnamespace,c.relname,c.relkind,c.relowner,c.relpersistence,
 c.reloptions,c.relrowsecurity,c.relforcerowsecurity,c.relhasrules,c.relhastriggers,
 c.relispartition,c.relpartbound::text,
 (SELECT jsonb_agg(to_jsonb(x) ORDER BY x.grantor,x.grantee,x.privilege_type,x.is_grantable)
 FROM aclexplode(coalesce(c.relacl,acldefault(CASE WHEN c.relkind='S' THEN 's'::"char" ELSE 'r'::"char" END,c.relowner))) x) AS acl,
 (SELECT jsonb_agg(jsonb_build_object('number',a.attnum,'name',a.attname,'type',a.atttypid,
 'modifier',a.atttypmod,'notnull',a.attnotnull,'dropped',a.attisdropped,'identity',a.attidentity,
 'generated',a.attgenerated,'collation',a.attcollation,'acl',(SELECT coalesce(jsonb_agg(to_jsonb(x) ORDER BY x.grantor,x.grantee,x.privilege_type,x.is_grantable),'[]'::jsonb) FROM aclexplode(a.attacl) x),
 'default',(SELECT pg_get_expr(d.adbin,d.adrelid,false) FROM pg_attrdef d WHERE d.adrelid=c.oid AND d.adnum=a.attnum)) ORDER BY a.attnum)
 FROM pg_attribute a WHERE a.attrelid=c.oid AND a.attnum>0) AS columns,
 CASE WHEN c.relkind IN('v','m') THEN pg_get_viewdef(c.oid,false) END AS view,
 (SELECT coalesce(jsonb_agg(jsonb_build_object('oid',r.oid,'name',r.rulename,'enabled',r.ev_enabled,
 'definition',pg_get_ruledef(r.oid,false)) ORDER BY r.rulename),'[]'::jsonb) FROM pg_rewrite r WHERE r.ev_class=c.oid) AS rules,
 (SELECT coalesce(jsonb_agg(jsonb_build_object('oid',t.oid,'function',t.tgfoid,'enabled',t.tgenabled,
 'definition',pg_get_triggerdef(t.oid,false)) ORDER BY t.oid),'[]'::jsonb) FROM pg_trigger t WHERE t.tgrelid=c.oid) AS triggers
 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
 WHERE n.nspname IN('pg_catalog','information_schema') AND c.oid<16384
), functions AS (
 SELECT p.oid,(to_jsonb(p)-'proacl') || jsonb_build_object('acl',
 (SELECT jsonb_agg(to_jsonb(x) ORDER BY x.grantor,x.grantee,x.privilege_type,x.is_grantable)
 FROM aclexplode(coalesce(p.proacl,acldefault('f',p.proowner))) x)) AS metadata
 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
 WHERE n.nspname='pg_catalog' AND p.oid<16384
)
SELECT jsonb_build_object('server_version_num',current_setting('server_version_num'),
 'namespaces',(SELECT jsonb_agg(jsonb_build_object('oid',n.oid,'name',n.nspname,'owner',n.nspowner,'acl',
 (SELECT jsonb_agg(to_jsonb(x) ORDER BY x.grantor,x.grantee,x.privilege_type,x.is_grantable) FROM aclexplode(coalesce(n.nspacl,acldefault('n',n.nspowner))) x)) ORDER BY n.oid)
 FROM pg_namespace n WHERE n.nspname IN('pg_catalog','information_schema')),
 'relations',(SELECT jsonb_agg(jsonb_build_object('oid',oid::bigint,'namespace',relnamespace::bigint,'name',relname,
 'sha256',encode(sha256(convert_to(to_jsonb(native)::text,'UTF8')),'hex')) ORDER BY oid) FROM native),
 'functions_sha256',(SELECT encode(sha256(convert_to(jsonb_agg(metadata ORDER BY oid)::text,'UTF8')),'hex') FROM functions))
