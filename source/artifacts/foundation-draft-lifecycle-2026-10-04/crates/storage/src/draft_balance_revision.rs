use crate::{PgRepository,StorageError};
impl PgRepository {
 /// New bank host requires explicitly reviewed native maintenance44. Never
 /// auto-migrate the shared database or treat a missing ledger as zero.
 pub async fn require_draft_balance_revision_for_backend(&self)->Result<(),StorageError>{
  let mut tx=self.pool.begin().await?;
  let m=sqlx::migrate::Migration::new(44,"simulation balance and draft intent".into(),sqlx::migrate::MigrationType::Simple,include_str!("../maintenance/0044_simulation_balance.sql").replace("__SCHEMA__",&self.namespace).replace("\r\n","\n").into(),false);
  let checksum:Vec<u8>=sqlx::query_scalar("SELECT simulation_balance_revision44_v1()").fetch_one(&mut *tx).await?;
  if checksum!=m.checksum.as_ref(){return Err(StorageError::Migration)}
  tx.commit().await?;Ok(())
 }
}
