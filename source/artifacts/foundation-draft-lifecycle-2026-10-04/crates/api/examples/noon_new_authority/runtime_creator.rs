//! Exact predecessor registrations only; never grants or renews a role.
use super::*;

pub(super) async fn validated_roles(tx: &mut Transaction<'_, Postgres>) -> Result<Vec<i64>, Error> {
    // PG16 records the non-superuser creator as an ADMIN-only member. Accept
    // that single reverse edge, or the original superuser-created zero-edge
    // case. Names alone never authorize an edge: bind the existing run, both
    // original registrations, purpose, expiry, source and worker completion.
    let roles: Vec<i64> = sqlx::query_scalar(r#"
      WITH expected(name,kind) AS (VALUES
        ('sf_noon_producer_20261029','PRODUCER'),
        ('sf_noon_worker_20261029','WORKER'))
      SELECT r.oid::bigint FROM expected e
      JOIN pg_roles r ON r.rolname=e.name
      JOIN sf_unified_noon_acceptance.payment_background_principals_v1 p
        ON p.role_name=r.rolname AND p.kind=e.kind
      JOIN sf_unified_noon_acceptance.payment_completion_candidate_fences f
        ON f.authority_id=r.rolname AND f.kind=e.kind
      JOIN sf_unified_noon_acceptance.payment_auto_runs_v1 run
        ON run.run_id=$1::uuid AND run.enabled
        AND run.producer_role='sf_noon_producer_20261029'
        AND run.worker_role='sf_noon_worker_20261029'
      WHERE p.active AND p.epoch=1 AND f.enabled AND f.epoch=1
        AND p.source_id='safeflow-team-simulation'
        AND p.implementation='foundation-unified-fictional-v1'
        AND p.valid_until<clock_timestamp() AND f.valid_until<clock_timestamp()
        AND r.rolcanlogin AND NOT r.rolinherit AND NOT r.rolsuper
        AND NOT r.rolcreatedb AND NOT r.rolcreaterole
        AND NOT r.rolreplication AND NOT r.rolbypassrls
        AND r.rolconfig IS NULL AND r.rolvaliduntil IS NULL AND r.rolconnlimit=-1
        AND EXISTS(SELECT 1 FROM sf_unified_noon_acceptance.payment_completion_logins_v1 l
          WHERE l.role_name=run.worker_role AND l.writer_version='completion-final-v1')
        AND (SELECT count(*) FROM pg_auth_members m WHERE m.roleid=r.oid OR m.member=r.oid)<=1
        AND NOT EXISTS(SELECT 1 FROM pg_auth_members m
          WHERE (m.roleid=r.oid OR m.member=r.oid) AND NOT (
            m.roleid=r.oid AND m.member=(SELECT oid FROM pg_roles WHERE rolname=$2)
            AND m.grantor=(SELECT oid FROM pg_roles WHERE rolname='postgres')
            AND m.admin_option AND NOT m.set_option AND NOT m.inherit_option))
        AND f.adoption_ref='sf-isolated-demo-policy-20261027-v1' AND f.scope=
            '{"accounts":[{"owner_id":"user_001","account_id":"account_001"},
              {"owner_id":"user_002","account_id":"account_002"},
              {"owner_id":"demo_user_li_v1","account_id":"demo_account_li_v1"}]}'::jsonb
      ORDER BY r.oid
    "#).bind(OLD_RUN).bind(OWNER).fetch_all(&mut **tx).await?;
    require(roles.len() == 2)?;
    Ok(roles)
}
