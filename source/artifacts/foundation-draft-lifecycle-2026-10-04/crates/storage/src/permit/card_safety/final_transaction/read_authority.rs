//! Final history-read authority; one READ COMMITTED statement snapshot.
use super::*;

// No VOLATILE helper, row lock or active-writer requirement. The existing
// public_card_final_objects_v1 is SQL STABLE and uses this statement snapshot.
// The marker also lets real protocol tests gate this actual final statement.
const CURRENT_READ_AUTHORITY: &str = r#"/* card_final_current_read_authority_v1 */
SELECT s.*,
  (SELECT coalesce(jsonb_agg(jsonb_build_array(account_id,owner_id,opening_cents)
      ORDER BY account_id),'[]'::jsonb) FROM transfer_accounts_v1) AS read_accounts,
  (SELECT coalesce(jsonb_agg(jsonb_build_array(owner_id,payee_id,recipient_account_id)
      ORDER BY owner_id,payee_id,recipient_account_id),'[]'::jsonb)
      FROM transfer_payee_routes_v1) AS read_routes,
  (EXISTS(SELECT 1 FROM transfer_accounts_v1 WHERE owner_id=$3 AND account_id=$4)
   AND NOT EXISTS(SELECT 1 FROM card_safety_account_fences_v1
       WHERE owner_id=$3 AND account_id=$4 AND revoked)
   AND NOT EXISTS(SELECT 1 FROM card_safety_session_fences_v1
       WHERE session_digest=$5 AND (owner_id<>$3 OR binding_version<>$6 OR revoked)))
       AS read_authorized,
  (SELECT to_jsonb(a) FROM
      (SELECT owner_id,source_id,implementation_revision,ordinal,source_revision,catalog_digest
       FROM public_card_catalog_adoptions_v1 WHERE account_id=$4 ORDER BY ordinal DESC LIMIT 1) a)
       AS read_adoption,
  EXISTS(SELECT 1 FROM public_card_final_deployment_v1
      WHERE singleton=1 AND reader_version='card-final-v1' AND source_id=$1
      AND implementation=$2
      AND schema_oid=(SELECT oid FROM pg_namespace WHERE nspname=current_schema())
      AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database())
      AND public_card_final_expected_digest_v1(deployment_id,object_digest)=public_card_final_objects_v1()) AS read_deployment
FROM public_unified_fictional_source_v1 s WHERE singleton=1
"#;

impl CardSafetyService<'_> {
    pub(super) async fn final_read_authorized(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        s: &SessionRef,
        b: &SessionBinding,
        input: &Input,
    ) -> Result<(), StorageError> {
        // Linearization point: PostgreSQL takes the RC snapshot for this
        // statement, not an earlier source/account/adoption SELECT. A withdrawal
        // committed before that snapshot is visible in ALL projections below.
        let row = sqlx::query(CURRENT_READ_AUTHORITY)
            .bind(UNIFIED_FICTIONAL_SOURCE)
            .bind(UNIFIED_FICTIONAL_IMPLEMENTATION)
            .bind(b.user_id.as_str())
            .bind(input.account_id.as_str())
            .bind(digest(s)?)
            .bind(db_version(b.binding_version)?)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(StorageError::DependencyUnavailable)?;
        self.source.check_card_read_snapshot(&row)?;
        if !row.try_get::<bool, _>("read_authorized")?
            || row.try_get::<Option<serde_json::Value>, _>("read_adoption")?.as_ref()
                != Some(&input.adoption)
        {
            return Err(StorageError::NotFound);
        }
        if !row.try_get::<bool, _>("read_deployment")? {
            return Err(StorageError::DecisionAssemblyChanged);
        }
        // Only the read-only COMMIT awaits after this; it grants no execution
        // rights. The caller retains synchronous session guards on both sides.
        // A later revocation can overlap response delivery; no network atomicity
        // is claimed, and repeatedly moving the last SELECT cannot provide it.
        self.guard(s, b)
    }
}
