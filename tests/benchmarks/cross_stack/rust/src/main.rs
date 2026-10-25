mod cpu_work;
mod pg_mode;

use axum::{
    body::to_bytes,
    extract::{DefaultBodyLimit, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::Read,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::timeout,
};
use tokio_postgres::{types::Type, NoTls};

const DEADLINE: Duration = Duration::from_secs(45);
const CPU_DEADLINE: Duration = Duration::from_secs(3);
#[derive(Clone, Copy)]
struct RequestDeadline(Instant);
struct Counters {
    active: AtomicUsize,
    waiting_timer: AtomicUsize,
    waiting_tcp: AtomicUsize,
    waiting_cpu: AtomicUsize,
    waiting_db: AtomicUsize,
    actual_cpu_jobs: Arc<AtomicUsize>,
}
impl Counters {
    fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            waiting_timer: AtomicUsize::new(0),
            waiting_tcp: AtomicUsize::new(0),
            waiting_cpu: AtomicUsize::new(0),
            waiting_db: AtomicUsize::new(0),
            actual_cpu_jobs: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn snapshot(&self) -> Value {
        json!({"active":self.active.load(Ordering::Relaxed), "waiting_timer":self.waiting_timer.load(Ordering::Relaxed), "waiting_tcp":self.waiting_tcp.load(Ordering::Relaxed), "waiting_cpu":self.waiting_cpu.load(Ordering::Relaxed), "waiting_db":self.waiting_db.load(Ordering::Relaxed), "actual_cpu_jobs":self.actual_cpu_jobs.load(Ordering::Relaxed),"allocation_total":"not_available_without_allocator_instrumentation"})
    }
}
struct Count<'a>(&'a AtomicUsize);
impl<'a> Count<'a> {
    fn new(value: &'a AtomicUsize) -> Self {
        value.fetch_add(1, Ordering::Relaxed);
        Self(value)
    }
}
impl Drop for Count<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
struct OwnedCount(Arc<AtomicUsize>);
impl OwnedCount {
    fn new(value: Arc<AtomicUsize>) -> Self {
        value.fetch_add(1, Ordering::Relaxed);
        Self(value)
    }
}
impl Drop for OwnedCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
struct App {
    active: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    cpu: Arc<Semaphore>,
    cpu_pending: Arc<Semaphore>,
    cpu_config: cpu_work::Config,
    cpu_trace: Arc<cpu_work::Trace>,
    counters: Counters,
    db: Option<Pool>,
    pg_mode: pg_mode::Mode,
    peer: u16,
}
fn bounded(name: &str, default: usize, low: usize, high: usize) -> usize {
    let value = std::env::var(name)
        .map(|s| s.parse::<usize>().expect(name))
        .unwrap_or(default);
    assert!((low..=high).contains(&value), "invalid {name}");
    value
}
fn text(status: StatusCode, body: impl Into<String>) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body.into(),
    )
        .into_response()
}
fn encoded(value: Value) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&value).expect("finite protocol values"),
    )
        .into_response()
}
fn compute(iterations: usize, seed: i64) -> i64 {
    let mut total = seed;
    for i in 0..iterations {
        total = (total * 33 + i as i64 + seed) % 100_000_000;
    }
    total
}
#[derive(Deserialize)]
struct Batch {
    iterations: usize,
    seed: i64,
    repeat: usize,
}
fn batch() {
    let mut source = String::new();
    if let Some(path) = std::env::args().nth(2) {
        source = std::fs::read_to_string(path).expect("batch input");
    } else {
        std::io::stdin()
            .read_to_string(&mut source)
            .expect("batch stdin");
    }
    let input: Batch = serde_json::from_str(&source).expect("batch JSON");
    assert!(
        (1..=1_000_000).contains(&input.iterations)
            && (0..=1024).contains(&input.seed)
            && input.repeat <= 100_000
    );
    let mut checksums = Vec::with_capacity(input.repeat);
    let started = Instant::now();
    for _ in 0..input.repeat {
        checksums.push(compute(input.iterations, input.seed));
    }
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    println!("{}", json!({"checksums":checksums,"compute_ms":elapsed}));
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("--batch") {
        batch();
        return Ok(());
    }
    let workers = bounded("BENCH_WORKERS", 1, 1, 4);
    let cpu_workers = bounded("BENCH_CPU_WORKERS", workers, 1, 4);
    let cpu_config = cpu_work::Config::from_env()?;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .worker_threads(workers)
        .max_blocking_threads(cpu_workers)
        .enable_all();
    if let Some(interval) = cpu_config.event_interval {
        builder.event_interval(interval);
    }
    let runtime = builder.build()?;
    runtime.block_on(serve(cpu_workers, cpu_config))
}
async fn serve(
    cpu_workers: usize,
    cpu_config: cpu_work::Config,
) -> Result<(), Box<dyn std::error::Error>> {
    let pg_mode = pg_mode::parse(std::env::var("BENCH_PG_MODE").ok().as_deref())?;
    let active = bounded("BENCH_MAX_ACTIVE", 1024, 1, 65536);
    let pending = bounded("BENCH_MAX_PENDING", 128, 1, 65536);
    let db_slots = bounded("BENCH_DB_POOL", 16, 1, 1024);
    let db = if let Ok(url) = std::env::var("BENCH_DB_URL") {
        let manager = Manager::from_config(
            url.parse()?,
            NoTls,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        Some(
            Pool::builder(manager)
                .max_size(db_slots)
                .wait_timeout(Some(Duration::from_secs(1)))
                .create_timeout(Some(Duration::from_secs(1)))
                .runtime(Runtime::Tokio1)
                .build()?,
        )
    } else {
        None
    };
    let app = Arc::new(App {
        active: Arc::new(Semaphore::new(active)),
        pending: Arc::new(Semaphore::new(pending)),
        cpu: Arc::new(Semaphore::new(cpu_workers)),
        cpu_pending: Arc::new(Semaphore::new(pending)),
        cpu_config,
        cpu_trace: Arc::new(cpu_work::Trace::new(cpu_config.trace)),
        counters: Counters::new(),
        pg_mode,
        db,
        peer: bounded("BENCH_PEER_PORT", 1, 1, 65535) as u16,
    });
    let routes = Router::new()
        .route("/metrics", get(metrics))
        .route("/ok", get(handler))
        .route("/json", post(handler))
        .route("/work", get(handler))
        .route("/timer", get(handler))
        .route("/wait", get(handler))
        .route("/tcp", get(handler))
        .route("/db", get(handler))
        .layer(DefaultBodyLimit::max(65536))
        .layer(middleware::from_fn_with_state(app.clone(), admission))
        .with_state(app.clone());
    let address = std::env::var("BENCH_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = TcpListener::bind(&address).await?;
    eprintln!(
        "{}",
        json!({"ready":true,"stack":"rust","async_workers":bounded("BENCH_WORKERS",1,1,4),"blocking_workers_max":cpu_workers,"db_pool_total":db_slots,"max_active":active,"max_pending":pending,"cpu_mode":cpu_config.mode.name(),"cpu_chunk":cpu_config.chunk,"event_interval":cpu_config.event_interval,"cpu_trace":cpu_config.trace,"request_deadline_ms":if cpu_config.mode == cpu_work::Mode::Legacy {45000} else {3000}})
    );
    axum::serve(listener, routes)
        .with_graceful_shutdown(shutdown())
        .await?;
    if let Some(db) = &app.db {
        db.close();
    }
    Ok(())
}
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM");
        tokio::select! { _ = terminate.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.expect("signal");
    }
}
async fn admission(State(app): State<Arc<App>>, mut request: Request, next: Next) -> Response {
    if request.uri().path() == "/metrics" {
        return next.run(request).await;
    }
    // One absolute deadline includes both admission queues and execution. It is
    // never renewed when a CPU permit becomes available.
    let deadline = if app.cpu_config.mode == cpu_work::Mode::Legacy {
        None
    } else {
        let deadline = Instant::now() + CPU_DEADLINE;
        request.extensions_mut().insert(RequestDeadline(deadline));
        Some(deadline)
    };
    let operation = async {
        let permit: OwnedSemaphorePermit =
            if let Ok(permit) = app.active.clone().try_acquire_owned() {
                permit
            } else {
                let Ok(_waiting) = app.pending.clone().try_acquire_owned() else {
                    return text(StatusCode::SERVICE_UNAVAILABLE, "request queue full");
                };
                match app.active.clone().acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => return text(StatusCode::SERVICE_UNAVAILABLE, "service stopping"),
                }
            };
        let _count = Count::new(&app.counters.active);
        let response = next.run(request).await;
        drop(permit);
        response
    };
    let response = if let Some(deadline) = deadline {
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), operation).await
    } else {
        timeout(DEADLINE, operation).await
    };
    response.unwrap_or_else(|_| text(StatusCode::GATEWAY_TIMEOUT, "request deadline"))
}
async fn metrics(State(app): State<Arc<App>>) -> Response {
    let mut snapshot = app.counters.snapshot();
    snapshot["cpu_mode"] = json!(app.cpu_config.mode.name());
    snapshot["cpu_chunk"] = json!(app.cpu_config.chunk);
    snapshot["event_interval"] = json!(app.cpu_config.event_interval);
    snapshot["cpu_permits_available"] = json!(app.cpu.available_permits());
    snapshot["cpu_pending_permits_available"] = json!(app.cpu_pending.available_permits());
    snapshot["active_permits_available"] = json!(app.active.available_permits());
    snapshot["pending_permits_available"] = json!(app.pending.available_permits());
    snapshot["cpu_trace"] = app.cpu_trace.snapshot();
    encoded(snapshot)
}
async fn handler(State(app): State<Arc<App>>, request: Request) -> Response {
    match execute(app, request).await {
        Ok(response) => response,
        Err((status, error)) => text(status, error),
    }
}
type Failure = (StatusCode, String);
fn failure(message: impl Into<String>) -> Failure {
    (StatusCode::INTERNAL_SERVER_ERROR, message.into())
}
fn invalid(message: impl Into<String>) -> Failure {
    (StatusCode::BAD_REQUEST, message.into())
}
async fn execute(app: Arc<App>, request: Request) -> Result<Response, Failure> {
    let path = request.uri().path().to_owned();
    let query = axum::extract::Query::<HashMap<String, String>>::try_from_uri(request.uri())
        .map_err(|e| invalid(e.to_string()))?
        .0;
    let rid = query
        .get("rid")
        .filter(|s| !s.is_empty() && s.len() <= 16 && s.bytes().all(|c| c.is_ascii_digit()))
        .ok_or_else(|| invalid("invalid rid"))?
        .clone();
    Ok(match path.as_str() {
        "/ok" => text(StatusCode::OK, format!("ok:{rid}")),
        "/json" => {
            let body = to_bytes(request.into_body(), 65536)
                .await
                .map_err(|e| invalid(e.to_string()))?;
            let mut input: Value =
                serde_json::from_slice(&body).map_err(|e| invalid(e.to_string()))?;
            let counter = input
                .get("counter")
                .and_then(Value::as_i64)
                .filter(|v| (-9_007_199_254_740_990..=9_007_199_254_740_990).contains(v))
                .ok_or_else(|| invalid("invalid counter"))?;
            // Move the decoded subtree; do not serialize cached request bytes.
            let data = input
                .as_object_mut()
                .ok_or_else(|| invalid("invalid envelope"))?
                .remove("data")
                .ok_or_else(|| invalid("missing data"))?;
            let mut envelope = serde_json::Map::with_capacity(3);
            envelope.insert("id".into(), Value::String(rid));
            envelope.insert("counter".into(), Value::from(counter + 1));
            envelope.insert("data".into(), data);
            encoded(Value::Object(envelope))
        }
        "/work" => {
            let work = query
                .get("work")
                .filter(|s| !s.is_empty() && s.len() <= 1000 && s.bytes().all(|c| c == b'x'))
                .ok_or_else(|| invalid("invalid work"))?
                .len()
                * 1000;
            let seed = query
                .get("seed")
                .filter(|s| !s.is_empty() && s.len() <= 1024 && s.bytes().all(|c| c == b's'))
                .ok_or_else(|| invalid("invalid seed"))?
                .len() as i64;
            let _count = Count::new(&app.counters.waiting_cpu);
            if app.cpu_config.mode != cpu_work::Mode::Legacy {
                let deadline = request
                    .extensions()
                    .get::<RequestDeadline>()
                    .ok_or_else(|| failure("missing absolute request deadline"))?
                    .0;
                let checksum = cpu_work::run(cpu_work::Input {
                    rid: &rid,
                    iterations: work,
                    seed,
                    deadline,
                    config: app.cpu_config,
                    cpu: app.cpu.clone(),
                    pending: app.cpu_pending.clone(),
                    actual: app.counters.actual_cpu_jobs.clone(),
                    trace: app.cpu_trace.clone(),
                })
                .await
                .map_err(|error| match error {
                    cpu_work::Failure::QueueFull => {
                        (StatusCode::SERVICE_UNAVAILABLE, "CPU queue full".into())
                    }
                    cpu_work::Failure::Closed => (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "CPU service stopping".into(),
                    ),
                    cpu_work::Failure::Deadline => {
                        (StatusCode::GATEWAY_TIMEOUT, "CPU deadline".into())
                    }
                    cpu_work::Failure::Cancelled => {
                        (StatusCode::REQUEST_TIMEOUT, "CPU cancelled".into())
                    }
                    cpu_work::Failure::Worker => failure("CPU worker failed"),
                })?;
                return Ok(text(StatusCode::OK, format!("{rid}:{checksum:08}")));
            }
            let queue = app
                .cpu_pending
                .clone()
                .try_acquire_owned()
                .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "CPU queue full".into()))?;
            let actual = OwnedCount::new(app.counters.actual_cpu_jobs.clone());
            let permit = app
                .cpu
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| failure("CPU service stopping"))?;
            drop(queue);
            let checksum = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let _actual = actual;
                compute(work, seed)
            })
            .await
            .map_err(|e| failure(e.to_string()))?;
            text(StatusCode::OK, format!("{rid}:{checksum:08}"))
        }
        "/timer" | "/wait" => {
            let _count = Count::new(&app.counters.waiting_timer);
            tokio::time::sleep(Duration::from_millis(if path == "/timer" {
                25
            } else {
                35000
            }))
            .await;
            text(
                StatusCode::OK,
                format!("{}:{rid}", if path == "/timer" { "timer" } else { "wait" }),
            )
        }
        "/tcp" => {
            let _count = Count::new(&app.counters.waiting_tcp);
            let operation = async {
                let mut stream = timeout(
                    Duration::from_secs(1),
                    TcpStream::connect(("127.0.0.1", app.peer)),
                )
                .await
                .map_err(|_| failure("TCP connect deadline"))?
                .map_err(|e| failure(e.to_string()))?;
                stream
                    .write_all(b"K")
                    .await
                    .map_err(|e| failure(e.to_string()))?;
                let mut reply = [0];
                stream
                    .read_exact(&mut reply)
                    .await
                    .map_err(|e| failure(e.to_string()))?;
                if reply != [75] {
                    return Err(failure("incorrect TCP reply"));
                }
                Ok(())
            };
            timeout(Duration::from_secs(40), operation)
                .await
                .map_err(|_| failure("TCP read deadline"))??;
            text(StatusCode::OK, format!("tcp:{rid}"))
        }
        "/db" => {
            let key: i32 = query
                .get("key")
                .and_then(|s| s.parse().ok())
                .filter(|k| (1..=1024).contains(k))
                .ok_or_else(|| invalid("invalid key"))?;
            let db = app
                .db
                .as_ref()
                .ok_or_else(|| failure("BENCH_DB_URL is required"))?;
            let _count = Count::new(&app.counters.waiting_db);
            if app.pg_mode == pg_mode::Mode::ResetEquivalent {
                let (id, label, score) = pg_mode::query_reset_equivalent(db, key)
                    .await
                    .map_err(failure)?;
                encoded(json!({"id":id,"label":label,"score":score,"rid":rid}))
            } else {
                let client = db.get().await.map_err(|e| failure(e.to_string()))?;
                let rows = timeout(
                    Duration::from_secs(1),
                    client.query_typed(
                        "SELECT id,label,score FROM cross_stack_items WHERE id=$1",
                        &[(&key, Type::INT4)],
                    ),
                )
                .await
                .map_err(|_| failure("query deadline"))?
                .map_err(|e| failure(e.to_string()))?;
                if rows.len() != 1 {
                    return Err(failure("incorrect database row count"));
                }
                let id: i32 = rows[0].try_get("id").map_err(|e| failure(e.to_string()))?;
                let label: String = rows[0]
                    .try_get("label")
                    .map_err(|e| failure(e.to_string()))?;
                let score: i32 = rows[0]
                    .try_get("score")
                    .map_err(|e| failure(e.to_string()))?;
                encoded(json!({"id":id,"label":label,"score":score,"rid":rid}))
            }
        }
        _ => text(StatusCode::NOT_FOUND, "not found"),
    })
}
