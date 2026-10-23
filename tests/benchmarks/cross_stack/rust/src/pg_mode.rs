//! Explicit session-isolation comparison; normal driver behavior is unchanged.
use deadpool_postgres::{Object, Pool};
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
}
