package ku.bench;

import com.fasterxml.jackson.databind.*;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.vertx.core.*;
import io.vertx.core.buffer.Buffer;
import io.vertx.core.http.*;
import io.vertx.pgclient.*;
import io.vertx.sqlclient.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicLong;

/** Normal Vert.x HTTP/PG and Jackson dynamic tree; no preencoded body cache. */
public final class Main {
  static final ObjectMapper JSON = new ObjectMapper().enable(DeserializationFeature.FAIL_ON_TRAILING_TOKENS);
  static final AtomicLong active = new AtomicLong(), waiting = new AtomicLong(), cpuJobs = new AtomicLong(), dbPending = new AtomicLong();
  static int env(String key, int value) { return Integer.parseInt(System.getenv().getOrDefault(key, "" + value)); }
  static long checksum(long iterations, long seed) {
    long total = seed;
    for (long i = 0; i < iterations; ++i) total = (total * 33 + i + seed) % 100000000;
    return total;
  }
  static void batch(String input) throws Exception {
    JsonNode n = JSON.readTree(Files.readAllBytes(Path.of(input)));
    int iterations = n.path("iterations").asInt(), seed = n.path("seed").asInt(), repeat = n.path("repeat").asInt();
    if (iterations < 1000 || iterations > 1000000 || seed < 1 || seed > 1024 || repeat < 0 || repeat > 1024) throw new IllegalArgumentException("invalid CPU input");
    long[] values = new long[repeat]; long start = System.nanoTime();
    for (int i = 0; i < repeat; i++) values[i] = checksum(iterations, seed);
    double ms = (System.nanoTime() - start) / 1e6;
    System.out.println(JSON.writeValueAsString(Map.of("checksums", values, "compute_ms", ms)));
  }
  static void send(HttpServerRequest r, int status, byte[] body, boolean json) {
    r.response().setStatusCode(status).putHeader("Content-Type", json ? "application/json; charset=utf-8" : "text/plain; charset=utf-8")
      .putHeader("Content-Length", "" + body.length).end(Buffer.buffer(body)).onComplete(x -> active.decrementAndGet());
  }
  static void text(HttpServerRequest r, int status, String body) { send(r, status, body.getBytes(java.nio.charset.StandardCharsets.UTF_8), false); }
  static void json(HttpServerRequest r, Object body) {
    try { send(r, 200, JSON.writeValueAsBytes(body), true); } catch (Exception e) { text(r, 500, "encode_error"); }
  }
  static Map<String, Object> metrics() {
    Runtime runtime = Runtime.getRuntime();
    Map<String, Object> gc = new LinkedHashMap<>();
    for (var collector : java.lang.management.ManagementFactory.getGarbageCollectorMXBeans())
      gc.put(collector.getName(), Map.of("collections", collector.getCollectionCount(), "collection_ms", collector.getCollectionTime()));
    return Map.of("active", active.get() - 1, "waiting_timer", waiting.get(), "waiting_tcp", 0,
      "actual_cpu_jobs", cpuJobs.get(), "database_pending", dbPending.get(),
      "runtime", Map.of("heap_used_bytes", runtime.totalMemory() - runtime.freeMemory(), "heap_committed_bytes", runtime.totalMemory(),
        "heap_max_bytes", runtime.maxMemory(), "available_processors", runtime.availableProcessors(), "gc", gc));
  }
  public static void main(String[] args) throws Exception {
    if (args.length > 0 && args[0].equals("--batch")) { batch(args.length > 1 ? args[1] : "batch.json"); return; }
    int workers = env("BENCH_WORKERS", 1);
    Vertx vertx = Vertx.vertx(new VertxOptions().setEventLoopPoolSize(workers).setWorkerPoolSize(workers));
    ThreadPoolExecutor cpu = new ThreadPoolExecutor(workers, workers, 0, TimeUnit.SECONDS, new ArrayBlockingQueue<>(128));
    String db = System.getenv("BENCH_DB_URL");
    Pool pool = db == null ? null : PgBuilder.pool().using(vertx).connectingTo(PgConnectOptions.fromUri(db).setCachePreparedStatements(false))
      .with(new io.vertx.sqlclient.PoolOptions().setMaxSize(env("BENCH_DB_POOL", 16)).setMaxWaitQueueSize(128).setConnectionTimeout(2).setConnectionTimeoutUnit(TimeUnit.SECONDS)).build();
    String[] addr = System.getenv().getOrDefault("BENCH_ADDR", "127.0.0.1:19090").split(":");
    Runtime.getRuntime().addShutdownHook(new Thread(() -> {
      cpu.shutdown(); try { cpu.awaitTermination(5, TimeUnit.SECONDS); } catch (InterruptedException ignored) { Thread.currentThread().interrupt(); }
      vertx.close().toCompletionStage().toCompletableFuture().join();
    }));
    // One shared listener per context: use every configured event loop, one total PG pool.
    CompletableFuture<?>[] listeners = new CompletableFuture<?>[workers];
    for (int i = 0; i < workers; i++) {
      listeners[i] = vertx.deployVerticle(new VerticleBase() {
        public io.vertx.core.Future<?> start() {
          return vertx.createHttpServer(new HttpServerOptions().setIdleTimeout(5).setMaxHeaderSize(4096).setMaxChunkSize(65536).setMaxFormAttributeSize(65536))
            .requestHandler(r -> {
              active.incrementAndGet(); String rid = Optional.ofNullable(r.getParam("rid")).orElse("");
              try {
                switch (r.path()) {
                  case "/metrics" -> json(r, metrics());
                  case "/ok" -> text(r, 200, "ok:" + rid);
                  case "/json" -> {
                    if (r.method() != HttpMethod.POST) { text(r, 405, "method_not_allowed"); break; }
                    final long[] bytes = {0}; Buffer body = Buffer.buffer();
                    r.handler(part -> { bytes[0] += part.length(); if (bytes[0] <= 65536) body.appendBuffer(part); });
                    r.endHandler(v -> {
                      try {
                        if (bytes[0] > 65536) { text(r, 400, "invalid_json_envelope"); return; }
                        JsonNode n = JSON.readTree(body.getBytes()); JsonNode counter = n.get("counter");
                        if (!n.isObject() || counter == null || !counter.isIntegralNumber() || !counter.canConvertToLong() || (counter.longValue() < -9007199254740990L || counter.longValue() > 9007199254740990L) || !n.has("data")) { text(r, 400, "invalid_json_envelope"); return; }
                        ObjectNode out = JSON.createObjectNode(); out.put("id", rid); out.put("counter", counter.longValue() + 1); out.set("data", n.get("data")); json(r, out);
                      } catch (Exception e) { text(r, 400, "invalid_json_envelope"); }
                    });
                    r.exceptionHandler(e -> active.decrementAndGet());
                  }
                  case "/timer" -> { waiting.incrementAndGet(); vertx.setTimer(25, id -> { waiting.decrementAndGet(); text(r, 200, "timer:" + rid); }); }
                  case "/work" -> {
                    int iterations = Optional.ofNullable(r.getParam("work")).orElse("").length() * 1000, seed = Optional.ofNullable(r.getParam("seed")).orElse("").length();
                    if (iterations < 1000 || iterations > 1000000 || seed < 1 || seed > 1024) { text(r, 400, "invalid_cpu_input"); break; }
                    Context context = Vertx.currentContext(); cpuJobs.incrementAndGet();
                    try { cpu.execute(() -> { long value = checksum(iterations, seed); cpuJobs.decrementAndGet(); context.runOnContext(v -> text(r, 200, rid + ":" + String.format(Locale.ROOT, "%08d", value))); }); }
                    catch (RejectedExecutionException e) { cpuJobs.decrementAndGet(); text(r, 503, "cpu_queue_full"); }
                  }
                  case "/db" -> {
                    int key; try { key = Integer.parseInt(r.getParam("key")); } catch (Exception e) { text(r, 400, "invalid_database_key"); break; }
                    if (key < 1 || key > 1024) { text(r, 400, "invalid_database_key"); break; }
                    if (pool == null) { text(r, 503, "database_not_configured"); break; }
                    dbPending.incrementAndGet(); pool.preparedQuery("SELECT id,label,score FROM cross_stack_items WHERE id=$1").execute(Tuple.of(key)).onComplete(result -> {
                      dbPending.decrementAndGet();
                      if (result.failed()) { text(r, 500, "database_query_error"); return; }
                      Row row = result.result().iterator().next(); json(r, Map.of("id", row.getInteger(0), "label", row.getString(1), "score", row.getInteger(2), "rid", rid));
                    });
                  }
                  default -> text(r, 404, "not_found");
                }
              } catch (Exception e) { text(r, 500, "handler_error"); }
            }).listen(Integer.parseInt(addr[1]), addr[0]);
        }
      }).toCompletionStage().toCompletableFuture();
    }
    CompletableFuture.allOf(listeners).join();
    System.err.println("Vert.x/Jackson ready workers=" + workers + " pg_total=" + env("BENCH_DB_POOL", 16));
  }
}
