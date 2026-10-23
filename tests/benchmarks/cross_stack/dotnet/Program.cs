using System.Diagnostics;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Threading.Channels;
using Microsoft.AspNetCore.Server.Kestrel.Core;
using Npgsql;

static class Bench {
  static long active, waiting, jobs, dbPending;
  static int Env(string name, int fallback) => int.Parse(Environment.GetEnvironmentVariable(name) ?? fallback.ToString());
  static long Checksum(int iterations, int seed) { long value = seed; for (int i = 0; i < iterations; i++) value = (value * 33 + i + seed) % 100000000; return value; }
  static async Task Text(HttpContext c, int code, string text) { var bytes = System.Text.Encoding.UTF8.GetBytes(text); c.Response.StatusCode = code; c.Response.ContentType = "text/plain; charset=utf-8"; c.Response.ContentLength = bytes.Length; await c.Response.Body.WriteAsync(bytes, c.RequestAborted); }
  static async Task Json(HttpContext c, object value) { byte[] bytes = JsonSerializer.SerializeToUtf8Bytes(value); c.Response.ContentType = "application/json; charset=utf-8"; c.Response.ContentLength = bytes.Length; await c.Response.Body.WriteAsync(bytes, c.RequestAborted); }
  record Job(int Iterations, int Seed, TaskCompletionSource<long> Result, CancellationToken Cancel);
  public static async Task Main(string[] args) {
    if (args.Length > 0 && args[0] == "--batch") {
      using var input = JsonDocument.Parse(await File.ReadAllBytesAsync(args.Length > 1 ? args[1] : "batch.json"));
      int iterations = input.RootElement.GetProperty("iterations").GetInt32(), seed = input.RootElement.GetProperty("seed").GetInt32(), repeat = input.RootElement.GetProperty("repeat").GetInt32();
      if (iterations < 1000 || iterations > 1000000 || seed < 1 || seed > 1024 || repeat < 0 || repeat > 32768) throw new ArgumentException("invalid CPU input");
      long[] checksums = new long[repeat]; var clock = Stopwatch.StartNew(); for (int i = 0; i < repeat; i++) checksums[i] = Checksum(iterations, seed); clock.Stop();
      Console.WriteLine(JsonSerializer.Serialize(new { checksums, compute_ms = clock.Elapsed.TotalMilliseconds })); return;
    }
    int workers = Env("BENCH_WORKERS", 1);
    var builder = WebApplication.CreateSlimBuilder(args); builder.Logging.ClearProviders();
    builder.WebHost.UseKestrel(o => { o.Limits.MaxRequestBodySize = 65536; o.Limits.KeepAliveTimeout = TimeSpan.FromSeconds(5); o.Limits.RequestHeadersTimeout = TimeSpan.FromSeconds(3); o.Limits.MaxRequestHeadersTotalSize = 4096; o.ConfigureEndpointDefaults(e => e.Protocols = HttpProtocols.Http1); });
    builder.WebHost.UseUrls("http://" + (Environment.GetEnvironmentVariable("BENCH_ADDR") ?? "127.0.0.1:19090"));
    NpgsqlDataSource? pool = null; string? uri = Environment.GetEnvironmentVariable("BENCH_DB_URL");
    if (uri is not null) { var u = new Uri(uri); var info = u.UserInfo.Split(':', 2); var cfg = new NpgsqlConnectionStringBuilder { Host = u.Host, Port = u.Port > 0 ? u.Port : 5432, Database = u.AbsolutePath.TrimStart('/'), Username = Uri.UnescapeDataString(info[0]), Password = info.Length > 1 ? Uri.UnescapeDataString(info[1]) : "", MaxPoolSize = Env("BENCH_DB_POOL", 16), MinPoolSize = 0, Timeout = 2, CommandTimeout = 45, MaxAutoPrepare = 0, SslMode = SslMode.Disable, NoResetOnClose = false }; pool = NpgsqlDataSource.Create(cfg.ConnectionString); }
    var channel = Channel.CreateBounded<Job>(new BoundedChannelOptions(128) { FullMode = BoundedChannelFullMode.Wait, SingleReader = workers == 1 });
    var app = builder.Build();
    Task[] cpuWorkers = Enumerable.Range(0, workers).Select(_ => Task.Run(async () => { await foreach (Job job in channel.Reader.ReadAllAsync()) { try { if (job.Cancel.IsCancellationRequested) job.Result.TrySetCanceled(job.Cancel); else job.Result.TrySetResult(Checksum(job.Iterations, job.Seed)); } finally { Interlocked.Decrement(ref jobs); } } })).ToArray();
    app.Use(async (c, next) => { Interlocked.Increment(ref active); try { await next(c); } catch (OperationCanceledException) when (c.RequestAborted.IsCancellationRequested) { } finally { Interlocked.Decrement(ref active); } });
    app.MapGet("/metrics", async (HttpContext c) => await Json(c, new { active = Interlocked.Read(ref active) - 1, waiting_timer = Interlocked.Read(ref waiting), waiting_tcp = 0, actual_cpu_jobs = Interlocked.Read(ref jobs), database_pending = Interlocked.Read(ref dbPending), gc_collections = new[] { GC.CollectionCount(0), GC.CollectionCount(1), GC.CollectionCount(2) }, allocated_bytes = GC.GetTotalAllocatedBytes(false), heap_bytes = GC.GetGCMemoryInfo().HeapSizeBytes }));
    app.MapGet("/ok", async (HttpContext c) => await Text(c, 200, "ok:" + c.Request.Query["rid"].ToString()));
    app.MapPost("/json", async (HttpContext c) => {
      JsonNode? root; try { root = await JsonNode.ParseAsync(c.Request.Body, cancellationToken: c.RequestAborted); } catch (JsonException) { await Text(c, 400, "invalid_json_envelope"); return; }
      if (root is not JsonObject envelope || !envelope.TryGetPropertyValue("counter", out var counterNode) || counterNode is not JsonValue counterValue || !counterValue.TryGetValue<long>(out long counter) || counter < -9007199254740990L || counter > 9007199254740990L || !envelope.ContainsKey("data")) { await Text(c, 400, "invalid_json_envelope"); return; }
      var data = envelope["data"]; envelope.Remove("data"); var result = new JsonObject { ["id"] = c.Request.Query["rid"].ToString(), ["counter"] = counter + 1, ["data"] = data }; await Json(c, result);
    });
    app.MapGet("/timer", async (HttpContext c) => { Interlocked.Increment(ref waiting); try { await Task.Delay(25, c.RequestAborted); await Text(c, 200, "timer:" + c.Request.Query["rid"].ToString()); } finally { Interlocked.Decrement(ref waiting); } });
    app.MapGet("/work", async (HttpContext c) => {
      int iterations = c.Request.Query["work"].ToString().Length * 1000, seed = c.Request.Query["seed"].ToString().Length;
      if (iterations < 1000 || iterations > 1000000 || seed < 1 || seed > 1024) { await Text(c, 400, "invalid_cpu_input"); return; }
      var result = new TaskCompletionSource<long>(TaskCreationOptions.RunContinuationsAsynchronously); Interlocked.Increment(ref jobs);
      if (!channel.Writer.TryWrite(new Job(iterations, seed, result, c.RequestAborted))) { Interlocked.Decrement(ref jobs); await Text(c, 503, "cpu_queue_full"); return; }
      long value = await result.Task.WaitAsync(c.RequestAborted); await Text(c, 200, c.Request.Query["rid"].ToString() + ":" + value.ToString("D8"));
    });
    app.MapGet("/db", async (HttpContext c) => {
      if (pool is null) { await Text(c, 503, "database_not_configured"); return; }
      if (!int.TryParse(c.Request.Query["key"], out int key) || key < 1 || key > 1024) { await Text(c, 400, "invalid_database_key"); return; }
      Interlocked.Increment(ref dbPending);
      try { await using var connection = await pool.OpenConnectionAsync(c.RequestAborted); await using var command = new NpgsqlCommand("SELECT id,label,score FROM cross_stack_items WHERE id=$1", connection); command.Parameters.AddWithValue(key); await using var row = await command.ExecuteReaderAsync(c.RequestAborted); if (!await row.ReadAsync(c.RequestAborted)) throw new InvalidOperationException("missing row"); await Json(c, new { id = row.GetInt32(0), label = row.GetString(1), score = row.GetInt32(2), rid = c.Request.Query["rid"].ToString() }); }
      catch (NpgsqlException) { await Text(c, 500, "database_query_error"); }
      finally { Interlocked.Decrement(ref dbPending); }
    });
    try { await app.RunAsync(); } finally { channel.Writer.TryComplete(); await Task.WhenAll(cpuWorkers); if (pool is not null) await pool.DisposeAsync(); }
  }
}
