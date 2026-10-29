//! Explicit session-isolation comparison; normal driver behavior is unchanged.
use deadpool_postgres::{Object, Pool};
use futures_core::Stream;
use std::future::poll_fn;
use std::time::Duration;
use tokio::time::{timeout_at, Instant};
use tokio_postgres::types::Type;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    ResetEquivalent,
}

pub fn parse(value: Option<&str>) -> Result<Mode, &'static str> {
    match value {
        None | Some("") | Some("normal") => Ok(Mode::Normal),
        Some("reset-equivalent") => Ok(Mode::ResetEquivalent),
        _ => Err("invalid BENCH_PG_MODE: expected normal or reset-equivalent"),
    }
}

/// An explicit API/parameter contract, independent of session reset mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    Int4,
    TypedTextKey,
}
impl Api {
    pub fn parse(value: Option<&str>) -> Result<Self, &'static str> {
        match value {
            None | Some("") | Some("normal") => Ok(Self::Int4),
            Some("typed-text-key") => Ok(Self::TypedTextKey),
            _ => Err("invalid BENCH_PG_API: expected normal or typed-text-key"),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Int4 => "normal",
            Self::TypedTextKey => "typed-text-key",
        }
    }
}

// A dropped future must not return an unreset session through Fast recycling.
// Object::take detaches it; ClientWrapper::drop aborts the connection task.
struct ResetLease(Option<Object>);
impl ResetLease {
    fn client(&self) -> &Object {
        self.0.as_ref().expect("live exclusive lease")
    }
    fn release_clean(mut self) {
        drop(self.0.take());
    }
}
impl Drop for ResetLease {
    fn drop(&mut self) {
        if let Some(client) = self.0.take() {
            drop(Object::take(client));
        }
    }
}

pub async fn query_reset_equivalent(pool: &Pool, key: i32) -> Result<(i32, String, i32), String> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(2);
    let acquired = timeout_at(started + Duration::from_millis(250), pool.get())
        .await
        .map_err(|_| "pool acquire deadline".to_owned())?
        .map_err(|e| e.to_string())?;
    query_reset_lease(ResetLease(Some(acquired)), key, deadline).await
}

async fn query_reset_lease(
    lease: ResetLease,
    key: i32,
    deadline: Instant,
) -> Result<(i32, String, i32), String> {
    let rows = timeout_at(
        deadline,
        lease.client().query_typed(
            "SELECT id,label,score FROM cross_stack_items WHERE id=$1",
            &[(&key, Type::INT4)],
        ),
    )
    .await
    .map_err(|_| "query deadline".to_owned())?
    .map_err(|e| e.to_string())?;
    if rows.len() != 1 {
        return Err("incorrect database row count".into());
    }
    // All values become owned before the exclusive session is reset/released.
    // Binary INT4/String remain the idiomatic result API, without extra JSON.
    let id = rows[0].try_get("id").map_err(|e| e.to_string())?;
    let label = rows[0].try_get("label").map_err(|e| e.to_string())?;
    let score = rows[0].try_get("score").map_err(|e| e.to_string())?;
    drop(rows);
    Ok(finish_reset(lease, deadline, (id, label, score)).await)
}

const TYPED_TEXT_SQL: &str = "SELECT id,label,score FROM cross_stack_items WHERE id=$1::int4";
type OwnedRow = (i32, String, i32);

// An empty/extra row or client conversion error is reported only after drain;
// a later PostgreSQL error therefore retains precedence. No Vec<Row> is built.
fn finish_exact_one(
    first: Option<Result<OwnedRow, String>>,
    extra: bool,
) -> Result<OwnedRow, String> {
    if extra {
        return Err("incorrect database row count".into());
    }
    first.ok_or_else(|| "incorrect database row count".to_owned())?
}
fn typed_query_error(error: tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(database) => format!("{error}: {}", database.message()),
        None => error.to_string(),
    }
}
async fn typed_text_row(
    lease: &ResetLease,
    key: &str,
    deadline: Instant,
    sql: &str,
) -> Result<OwnedRow, String> {
    timeout_at(deadline, async {
        let stream = lease
            .client()
            .query_typed_raw(sql, [(&key, Type::TEXT)])
            .await
            .map_err(typed_query_error)?;
        let mut stream = std::pin::pin!(stream);
        let mut first = None;
        let mut extra = false;
        while let Some(row) = poll_fn(|context| stream.as_mut().poll_next(context)).await {
            // A wire/SQL error ends this operation and evicts the lease. In
            // particular, never spin on a disconnected stream's repeated error.
            let row = row.map_err(typed_query_error)?;
            if first.is_none() {
                first = Some((|| {
                    Ok((
                        row.try_get("id").map_err(typed_query_error)?,
                        row.try_get("label").map_err(typed_query_error)?,
                        row.try_get("score").map_err(typed_query_error)?,
                    ))
                })());
            } else {
                extra = true;
            }
            // Row drops here; only the first row's independently owned scalars
            // and String remain while later messages are drained.
        }
        finish_exact_one(first, extra)
    })
    .await
    .map_err(|_| "query deadline".to_owned())?
}

pub async fn query_typed_text_key(pool: &Pool, key: &str, mode: Mode) -> Result<OwnedRow, String> {
    let started = Instant::now();
    let (client, deadline) = if mode == Mode::ResetEquivalent {
        let client = timeout_at(started + Duration::from_millis(250), pool.get())
            .await
            .map_err(|_| "pool acquire deadline".to_owned())?
            .map_err(|error| error.to_string())?;
        (client, started + Duration::from_secs(2))
    } else {
        // The pool's existing 1s acquire/create configuration is unchanged.
        let client = pool.get().await.map_err(|error| error.to_string())?;
        (client, Instant::now() + Duration::from_secs(1))
    };
    let lease = ResetLease(Some(client));
    let owned = typed_text_row(&lease, key, deadline, TYPED_TEXT_SQL).await?;
    if mode == Mode::ResetEquivalent {
        Ok(finish_reset(lease, deadline, owned).await)
    } else {
        lease.release_clean();
        Ok(owned)
    }
}

async fn finish_reset(
    lease: ResetLease,
    deadline: Instant,
    owned: (i32, String, i32),
) -> (i32, String, i32) {
    // batch_execute completes at ReadyForQuery, not just command submission.
    // Reset never receives a fresh 2s budget. A failure drops/evicts the lease,
    // but the detached result remains usable; no query is retried or replayed.
    if matches!(
        timeout_at(deadline, lease.client().batch_execute("DISCARD ALL")).await,
        Ok(Ok(()))
    ) {
        lease.release_clean();
    }
    owned
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::{Manager, ManagerConfig, RecyclingMethod, Runtime};
    use tokio_postgres::NoTls;

    #[test]
    fn only_explicit_mode_enables_reset() {
        for value in [None, Some(""), Some("normal")] {
            assert_eq!(parse(value), Ok(Mode::Normal));
        }
        assert_eq!(parse(Some("reset-equivalent")), Ok(Mode::ResetEquivalent));
        for value in ["reset", "RESET-EQUIVALENT", " normal "] {
            assert!(parse(Some(value)).is_err());
        }
    }

    #[test]
    fn typed_text_api_is_explicit_and_exact_one_finishes_after_drain() {
        assert_eq!(Api::parse(None), Ok(Api::Int4));
        assert_eq!(Api::parse(Some("normal")), Ok(Api::Int4));
        assert_eq!(Api::parse(Some("typed-text-key")), Ok(Api::TypedTextKey));
        assert!(Api::parse(Some("typed")).is_err());
        assert!(finish_exact_one(None, false).is_err());
        assert!(finish_exact_one(Some(Ok((1, "owned".into(), 2))), true).is_err());
        assert_eq!(
            finish_exact_one(Some(Err("field error".into())), false),
            Err("field error".into())
        );
        assert_eq!(
            finish_exact_one(Some(Ok((1, "owned".into(), 2))), false).unwrap(),
            (1, "owned".into(), 2)
        );
    }

    fn private_pool() -> Pool {
        let url = std::env::var("BENCH_PG_TEST_URL")
            .expect("explicit ignored live tests require BENCH_PG_TEST_URL");
        let manager = Manager::from_config(
            url.parse().expect("private fixture configuration"),
            NoTls,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        Pool::builder(manager)
            .max_size(1)
            .wait_timeout(Some(Duration::from_millis(250)))
            .create_timeout(Some(Duration::from_secs(2)))
            .runtime(Runtime::Tokio1)
            .build()
            .unwrap()
    }

    fn run_live(future: impl std::future::Future<Output = ()>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), future)
                .await
                .expect("bounded live PG test deadline");
        });
    }

    // Uses only the explicit private benchmark fixture. Session state is local
    // to one lease; neither user data nor shared schema is modified.
    #[test]
    #[ignore = "requires BENCH_PG_TEST_URL pointing at the private fixture"]
    fn live_same_session_reset_and_owned_result_after_reset_failure() {
        run_live(async {
            let pool = private_pool();
            let client = pool.get().await.unwrap();
            let pid: i32 = client
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get(0);
            client.batch_execute("SET application_name='ku_reset_probe'; CREATE TEMP TABLE ku_reset_probe(value int)").await.unwrap();
            drop(client);
            let owned = query_reset_equivalent(&pool, 17).await.unwrap();
            assert_eq!(owned.0, 17);
            assert!(!owned.1.is_empty());
            let client = pool.get().await.unwrap();
            let row = client.query_one("SELECT pg_backend_pid(),current_setting('application_name'),to_regclass('pg_temp.ku_reset_probe') IS NULL", &[]).await.unwrap();
            assert_eq!(
                row.get::<_, i32>(0),
                pid,
                "successful reset must use the original lease"
            );
            assert_ne!(row.get::<_, String>(1), "ku_reset_probe");
            assert!(row.get::<_, bool>(2));
            drop(row);
            // Once the row is owned, an exhausted reset deadline evicts the
            // session and returns that result; it cannot re-run business SQL.
            let expected = owned.clone();
            let actual = finish_reset(
                ResetLease(Some(client)),
                Instant::now() - Duration::from_secs(1),
                owned,
            )
            .await;
            assert_eq!(actual, expected);
            assert_eq!(pool.status().size, 0);
            let replacement = pool.get().await.unwrap();
            let next_pid: i32 = replacement
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get(0);
            assert_ne!(next_pid, pid);
            drop(replacement);
            pool.close();
        });
    }

    #[test]
    #[ignore = "requires BENCH_PG_TEST_URL pointing at the private fixture"]
    fn live_cancelled_future_evicts_unreset_lease() {
        run_live(async {
            let pool = private_pool();
            let client = pool.get().await.unwrap();
            let guard = ResetLease(Some(client));
            let (ready, notified) = tokio::sync::oneshot::channel();
            let operation = tokio::spawn(async move {
                let _lease = guard;
                ready.send(()).unwrap();
                std::future::pending::<()>().await;
            });
            notified.await.unwrap();
            operation.abort();
            assert!(operation.await.unwrap_err().is_cancelled());
            assert_eq!(pool.status().size, 0);
            pool.close();
        });
    }
    #[test]
    #[ignore = "requires BENCH_PG_TEST_URL pointing at the private fixture"]
    fn live_typed_text_key_preserves_business_and_evicts_late_sql_error() {
        run_live(async {
            let pool = private_pool();
            let expected = query_reset_equivalent(&pool, 17).await.unwrap();
            for mode in [Mode::Normal, Mode::ResetEquivalent] {
                assert_eq!(
                    query_typed_text_key(&pool, "17", mode).await.unwrap(),
                    expected
                );
            }
            let client = pool.get().await.unwrap();
            let lease = ResetLease(Some(client));
            let error=typed_text_row(&lease,"17",Instant::now()+Duration::from_secs(2),
                "SELECT (1/(2-n))::int4 AS id,'owned'::text AS label,1::int4 AS score FROM generate_series(1,2) n WHERE $1::text IS NOT NULL")
                .await.unwrap_err();
            assert!(error.contains("division by zero"), "{error}");
            drop(lease);
            assert_eq!(
                pool.status().size,
                0,
                "SQL error cannot recycle the old lease"
            );
            assert_eq!(
                query_typed_text_key(&pool, "17", Mode::ResetEquivalent)
                    .await
                    .unwrap(),
                expected
            );
            pool.close();
        });
    }
}
