use super::*;
use crate::runtime::task::{current_blocking_owner_cancelled, TaskHandle, TaskRuntime};
use std::{
    cell::RefCell,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
};

#[derive(Default)]
struct Sends {
    business: AtomicUsize,
    reset: AtomicUsize,
}

impl Sends {
    fn counts(&self) -> (usize, usize) {
        (
            self.business.load(Ordering::Acquire),
            self.reset.load(Ordering::Acquire),
        )
    }
}

struct PoolObserver {
    registered: SyncSender<()>,
    sends: Arc<Sends>,
}

thread_local! {
    static OBSERVER: RefCell<Option<PoolObserver>> = const { RefCell::new(None) };
}

struct ObserverGuard;

impl Drop for ObserverGuard {
    fn drop(&mut self) {
        OBSERVER.with(|slot| drop(slot.replace(None)));
    }
}

pub(super) fn observe_waiter_registration() {
    OBSERVER.with(|slot| {
        if let Some(observer) = slot.borrow().as_ref() {
            // The pool mutex is held here. An observation cannot block or
            // publish another pool wake.
            let _ = observer.registered.try_send(());
        }
    });
}

pub(super) fn observe_send(sql: &CStr) {
    OBSERVER.with(|slot| {
        if let Some(observer) = slot.borrow().as_ref() {
            let counter = if sql == c"DISCARD ALL" {
                &observer.sends.reset
            } else {
                &observer.sends.business
            };
            counter.fetch_add(1, Ordering::Release);
        }
    });
}

struct Actor {
    task: TaskHandle,
    queued: Receiver<()>,
    outcome: Receiver<Value>,
    sends: Arc<Sends>,
}

fn start(runtime: &TaskRuntime, query: PreparedQuery) -> Actor {
    let (registered, queued) = mpsc::sync_channel(1);
    let (finished, outcome) = mpsc::sync_channel(1);
    let sends = Arc::new(Sends::default());
    let observer = PoolObserver {
        registered,
        sends: Arc::clone(&sends),
    };
    let blocking = runtime.clone();
    let task = runtime
        .spawn(move || {
            blocking.run_blocking(
                move || {
                    OBSERVER.with(|slot| assert!(slot.replace(Some(observer)).is_none()));
                    let _guard = ObserverGuard;
                    let result = query.execute(&current_blocking_owner_cancelled);
                    // Publish what the blocking worker observed after actual
                    // PG cleanup, separately from the Task's terminal result.
                    let _ = finished.try_send(result.clone());
                    Ok(result)
                },
                Span::default(),
            )
        })
        .expect("submit PG test actor");
    Actor {
        task,
        queued,
        outcome,
        sends,
    }
}

fn plan(
    client: &Arc<Client>,
    sql: &str,
    params: &[Value],
    deadline: Option<Instant>,
) -> PreparedQuery {
    prepare_query(
        "query",
        &[
            Value::PgClient(Arc::clone(client)),
            Value::String(sql.into()),
            Value::Array(params.to_vec()),
        ],
        Span::default(),
        deadline,
        true,
        &|| false,
    )
    .unwrap()
    .unwrap()
}

fn rows(value: Value) -> Arc<Rows> {
    let Value::Result { ok: true, value } = value else {
        panic!("expected successful PG result, got {value}");
    };
    let Value::PgResult(rows) = *value else {
        panic!("missing PG result owner")
    };
    rows
}

fn read(rows: &Rows, method: &str, col: i64) -> Value {
    rows.read(method, 0, col).unwrap()
}

struct Control {
    connection: Connection,
    schema: Option<String>,
    lock_key: i32,
}

impl Control {
    fn new(conninfo: &str, schema: &str, lock_key: i32) -> Self {
        let connection = Connection::connect(
            &CString::new(conninfo).unwrap(),
            Instant::now() + Duration::from_secs(2),
            &mut || Ok(()),
        )
        .unwrap();
        let mut control = Self {
            connection,
            schema: None,
            lock_key,
        };
        control.query(&format!("CREATE SCHEMA {schema}"));
        control.schema = Some(schema.into());
        control.query(&format!(
            "CREATE TABLE {schema}.waiter_send (token text NOT NULL)"
        ));
        control
    }

    fn query(&mut self, sql: &str) -> Tabular {
        self.connection
            .execute(
                &CString::new(sql).unwrap(),
                &[],
                Instant::now() + Duration::from_secs(2),
                &mut || Ok(()),
            )
            .expect("execute disposable private PG control statement")
    }

    fn scalar(&mut self, sql: &str) -> String {
        self.query(sql)
            .cells
            .into_iter()
            .next()
            .flatten()
            .expect("one scalar")
    }

    fn holder_waiting(&mut self, pid: i64) -> bool {
        self.scalar(&format!(
            "SELECT count(*) FROM pg_stat_activity WHERE pid={pid} AND state='active' \
             AND wait_event_type='Lock' AND wait_event='advisory'"
        )) == "1"
    }

    fn unlock(&mut self) {
        let key = self.lock_key;
        assert_eq!(
            self.scalar(&format!("SELECT pg_advisory_unlock(2404,{key})")),
            "t"
        );
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        // Names come exclusively from the current test process. Closing the
        // control connection also releases the lock if this cleanup fails.
        let end = Instant::now() + Duration::from_secs(2);
        let _ = self
            .connection
            .execute(c"SELECT pg_advisory_unlock_all()", &[], end, &mut || Ok(()));
        if let Some(schema) = &self.schema {
            let sql = CString::new(format!("DROP SCHEMA {schema} CASCADE")).unwrap();
            let _ = self.connection.execute(&sql, &[], end, &mut || Ok(()));
        }
    }
}

fn await_holder_lock(control: &mut Control, pid: i64) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !control.holder_waiting(pid) {
        assert!(
            Instant::now() < deadline,
            "holder never reached advisory lock"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn pool_work(client: &Client) -> (usize, usize) {
    let pool = client.pool();
    (pool.active, pool.waiting)
}

fn scenario(conninfo: &str, cancel: bool) {
    let before = budget::test_usage();
    let key = i32::try_from(std::process::id()).unwrap();
    let schema = format!("ku_async_pg_hwn_{key}_{}", u8::from(cancel));
    let config = Config {
        conninfo: Conninfo(Some(CString::new(conninfo).unwrap())),
        connections: 1,
        waiters: 2,
        connect_timeout: Duration::from_secs(2),
        acquire_timeout: Duration::from_secs(6),
        query_timeout: Duration::from_secs(8),
    };
    let client = Client::open(config, None, &|| false).unwrap();
    let mut control = Control::new(conninfo, &schema, key);
    control.query(&format!("SELECT pg_advisory_lock(2404,{key})"));

    let initial = rows(
        plan(
            &client,
            "SELECT pg_backend_pid(), current_setting('work_mem')",
            &[],
            None,
        )
        .execute(&|| false),
    );
    let Value::Int(pid) = read(&initial, "value_int", 0) else {
        panic!("invalid PG backend PID")
    };
    let expected_setting = read(&initial, "value", 1);
    let runtime = TaskRuntime::with_limits(3, 8, 3, 8, 8);
    let holder_sql = format!(
        "SELECT pg_backend_pid(), pg_advisory_xact_lock(2404,{key})::text, \
         set_config('work_mem', CASE WHEN current_setting('work_mem')='128kB' THEN '256kB' ELSE '128kB' END, false)"
    );
    let h = start(&runtime, plan(&client, &holder_sql, &[], None));
    await_holder_lock(&mut control, pid);

    let waiter_sql = format!("INSERT INTO {schema}.waiter_send VALUES ($1) RETURNING token");
    let original_deadline = Instant::now() + Duration::from_secs(2);
    let w = start(
        &runtime,
        plan(
            &client,
            &waiter_sql,
            &[Value::String("waiter-must-not-send".into())],
            Some(original_deadline),
        ),
    );
    w.queued
        .recv_timeout(Duration::from_secs(2))
        .expect("W registered");
    let n = start(
        &runtime,
        plan(
            &client,
            "SELECT pg_backend_pid(), current_setting('work_mem'), $1::text",
            &[Value::String("next-after-W".into())],
            None,
        ),
    );
    n.queued
        .recv_timeout(Duration::from_secs(2))
        .expect("N registered while W was waiting");
    assert_eq!(pool_work(&client), (1, 2));
    assert!(
        Instant::now() < original_deadline,
        "both waiters must queue before W expires"
    );
    assert!(control.holder_waiting(pid));
    if cancel {
        assert!(w.task.cancel());
    }
    let finished = w
        .outcome
        .recv_timeout(Duration::from_secs(3))
        .expect("W worker observed terminal outcome");
    let error = if cancel {
        CANCELLED.kind
    } else {
        ACQUIRE_TIMEOUT.kind
    };
    assert_eq!(
        finished,
        result_error(if cancel { CANCELLED } else { ACQUIRE_TIMEOUT }),
        "W outcome: {error}"
    );
    assert_eq!(w.sends.counts(), (0, 0));
    assert_eq!(n.sends.counts(), (0, 0));
    assert_eq!(pool_work(&client), (1, 1));
    assert!(control.holder_waiting(pid), "H must still hold the lease");
    assert_eq!(
        control.scalar(&format!("SELECT count(*) FROM {schema}.waiter_send")),
        "0"
    );

    control.unlock();
    let h_rows = rows(h.task.await_result().unwrap());
    let n_rows = rows(n.task.await_result().unwrap());
    assert_eq!(read(&h_rows, "value_int", 0), Value::Int(pid));
    assert_ne!(read(&h_rows, "value", 2), expected_setting);
    assert_eq!(read(&n_rows, "value_int", 0), Value::Int(pid));
    assert_eq!(read(&n_rows, "value", 1), expected_setting);
    assert_eq!(
        read(&n_rows, "value", 2),
        Value::String("next-after-W".into())
    );
    assert_eq!(h.sends.counts(), (1, 1));
    assert_eq!(n.sends.counts(), (1, 1));
    assert_eq!(w.sends.counts(), (0, 0));
    assert_eq!(
        control.scalar(&format!("SELECT count(*) FROM {schema}.waiter_send")),
        "0"
    );
    drop((h, w, n));
    runtime
        .cancel_all_and_wait(Duration::from_secs(2))
        .expect("retire PG test work");
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(
        (
            snapshot.active_tasks,
            snapshot.queued_blocking_jobs,
            snapshot.running_blocking_jobs
        ),
        (0, 0, 0)
    );
    {
        let pool = client.pool();
        assert_eq!(
            (pool.active, pool.waiting, pool.idle.len(), pool.connecting),
            (0, 0, 1, false)
        );
    }
    client.close();
    assert_eq!(budget::test_usage(), before);
}

fn queued_before_acquire_timeout(conninfo: &str) {
    let before = budget::test_usage();
    let key = i32::try_from(std::process::id()).unwrap();
    let schema = format!("ku_async_pg_worker_queue_{key}");
    let timeout = Duration::from_millis(500);
    let client = Client::open(
        Config {
            conninfo: Conninfo(Some(CString::new(conninfo).unwrap())),
            connections: 1,
            waiters: 1,
            connect_timeout: Duration::from_secs(2),
            acquire_timeout: timeout,
            query_timeout: Duration::from_secs(8),
        },
        None,
        &|| false,
    )
    .unwrap();
    let mut control = Control::new(conninfo, &schema, key);

    let runtime = TaskRuntime::with_limits(2, 4, 1, 4, 4);
    let (entered, running) = mpsc::sync_channel(1);
    let (release, gate) = mpsc::sync_channel(1);
    let blocking = runtime.clone();
    let blocker = runtime
        .spawn(move || {
            blocking.run_blocking(
                move || {
                    entered.send(()).unwrap();
                    gate.recv_timeout(Duration::from_secs(3)).unwrap();
                    Ok(Value::Null)
                },
                Span::default(),
            )
        })
        .unwrap();
    running.recv_timeout(Duration::from_secs(1)).unwrap();

    let sql = format!("INSERT INTO {schema}.waiter_send VALUES ($1) RETURNING token");
    let query = plan(
        &client,
        &sql,
        &[Value::String("expired-before-worker-started".into())],
        None,
    );
    let after_preparation = Instant::now();
    let w = start(&runtime, query);
    let queue_deadline = after_preparation + timeout;
    while runtime.snapshot().unwrap().queued_blocking_jobs != 1 {
        assert!(
            Instant::now() < queue_deadline,
            "W did not queue before expiry"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(pool_work(&client), (0, 0));

    // The one-worker gate makes crossing the configured acquire deadline
    // observable without relying on a competing PG waiter or pool activity.
    while Instant::now() < queue_deadline {
        thread::sleep(queue_deadline.saturating_duration_since(Instant::now()));
    }
    release.send(()).unwrap();
    blocker.await_result().unwrap();
    let finished = w.outcome.recv_timeout(Duration::from_secs(2)).unwrap();
    let sent = w.sends.counts();
    let count = control.scalar(&format!("SELECT count(*) FROM {schema}.waiter_send"));
    assert_eq!(
        (finished, sent, count),
        (result_error(ACQUIRE_TIMEOUT), (0, 0), "0".into()),
        "an expired queued W must not obtain the still-idle lease or send SQL"
    );
    w.task.await_result().unwrap();
    runtime.cancel_all_and_wait(Duration::from_secs(2)).unwrap();
    assert_eq!(pool_work(&client), (0, 0));
    client.close();
    assert_eq!(budget::test_usage(), before);
}

#[test]
#[ignore = "explicit opt-in: requires a disposable private PostgreSQL database"]
fn blocking_pg_waiter_hands_lease_to_next_without_sending_after_cancel_or_expiry() {
    let path = std::env::var_os("KU_PG_TEST_CONNINFO_FILE")
        .map(PathBuf::from)
        .expect("set KU_PG_TEST_CONNINFO_FILE to a private disposable test database");
    let conninfo = std::fs::read_to_string(path).expect("read private PostgreSQL fixture conninfo");
    scenario(conninfo.trim(), true);
    scenario(conninfo.trim(), false);
    queued_before_acquire_timeout(conninfo.trim());
}
