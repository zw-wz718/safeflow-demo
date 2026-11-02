//! Small-memory checks against the actual locked SQLx implementation.
//! No oversized allocation, shared database, or patched dependency cache.
use sqlx::{
    encode::IsNull,
    error::BoxDynError,
    postgres::{PgArgumentBuffer, PgArguments, PgConnectOptions, PgTypeInfo},
    Arguments, Connection, Encode, Postgres, Type,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Clone)]
struct Probe {
    hint: usize,
    fail: bool,
    calls: Arc<AtomicUsize>,
}
impl Type<Postgres> for Probe {
    fn type_info() -> PgTypeInfo {
        <i32 as Type<Postgres>>::type_info()
    }
}
impl<'q> Encode<'q, Postgres> for Probe {
    fn encode_by_ref(&self, buffer: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        buffer.extend_from_slice(&17_i32.to_be_bytes());
        if self.fail {
            return Err("deliberate encoder failure after partial write".into());
        }
        Ok(IsNull::No)
    }
    fn size_hint(&self) -> usize {
        self.hint
    }
}
fn probe(hint: usize, fail: bool) -> Probe {
    Probe {
        hint,
        fail,
        calls: Arc::new(AtomicUsize::new(0)),
    }
}

#[test]
fn oversized_hint_is_rejected_before_encoding_without_allocation() {
    for hint in [i32::MAX as usize + 1, u32::MAX as usize, usize::MAX] {
        let value = probe(hint, false);
        let mut args = PgArguments::default();
        Arguments::add(&mut args, 11_i32).unwrap();
        let before = format!("{args:?}");
        assert!(Arguments::add(&mut args, value.clone()).is_err());
        assert_eq!(value.calls.load(Ordering::SeqCst), 0);
        assert_eq!(Arguments::len(&args), 1);
        assert_eq!(format!("{args:?}"), before);
    }
}

#[test]
fn accepted_hint_boundary_and_encoder_failure_preserve_arguments() {
    let mut args = PgArguments::default();
    // A size_hint does not allocate that size. The actual encoding is four bytes.
    let value = probe(i32::MAX as usize, false);
    Arguments::add(&mut args, value.clone()).unwrap();
    assert_eq!(value.calls.load(Ordering::SeqCst), 1);
    let before = format!("{args:?}");
    let broken = probe(0, true);
    assert!(Arguments::add(&mut args, broken.clone()).is_err());
    assert_eq!(broken.calls.load(Ordering::SeqCst), 1);
    assert_eq!(format!("{args:?}"), before);
    assert_eq!(Arguments::len(&args), 1);
    Arguments::add(&mut args, 19_i32).unwrap();
    assert_eq!(Arguments::len(&args), 2);
}

#[tokio::test]
#[ignore = "requires this task's new isolated loopback PostgreSQL database"]
async fn encode_errors_never_execute_and_savepoint_rollback_remains_atomic() {
    let options: PgConnectOptions = std::env::var("SAFEFLOW_TEST_DATABASE_URL")
        .expect("explicit isolated test URL required")
        .parse()
        .unwrap();
    assert_eq!(options.get_host(), "127.0.0.1");
    assert_eq!(options.get_database(), Some("sqlx_unified_20261021"));
    let mut conn = sqlx::PgConnection::connect_with(&options).await.unwrap();
    sqlx::query("CREATE TEMP TABLE sqlx_security_probe (value integer NOT NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    for value in [probe(usize::MAX, false), probe(0, true)] {
        let error = sqlx::query("INSERT INTO sqlx_security_probe VALUES ($1)")
            .bind(value)
            .execute(&mut conn)
            .await
            .unwrap_err();
        assert!(matches!(error, sqlx::Error::Encode(_)));
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlx_security_probe")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("INSERT INTO sqlx_security_probe VALUES (11)")
        .execute(&mut *tx)
        .await
        .unwrap();
    {
        let mut savepoint = tx.begin().await.unwrap();
        sqlx::query("INSERT INTO sqlx_security_probe VALUES (22)")
            .execute(&mut *savepoint)
            .await
            .unwrap();
        assert!(sqlx::query("INSERT INTO sqlx_security_probe VALUES ($1)")
            .bind(probe(0, true))
            .execute(&mut *savepoint)
            .await
            .is_err());
        // Dropping an unfinished nested transaction queues ROLLBACK TO SAVEPOINT.
    }
    let values: Vec<i32> =
        sqlx::query_scalar("SELECT value FROM sqlx_security_probe ORDER BY value")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    assert_eq!(values, [11]);
    tx.rollback().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlx_security_probe")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let mut args = PgArguments::default();
    Arguments::add(&mut args, 31_i32).unwrap();
    assert!(Arguments::add(&mut args, probe(0, true)).is_err());
    Arguments::add(&mut args, 37_i32).unwrap();
    let sum: i32 = sqlx::query_scalar_with("SELECT $1::int4 + $2::int4", args)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(sum, 68);
    conn.close().await.unwrap();
}
