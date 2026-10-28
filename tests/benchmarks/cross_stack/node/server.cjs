'use strict';
// A service is either one HTTP process or cluster primary + N HTTP processes.
// Every HTTP process owns one CPU worker; CPU work never blocks its event loop.
const cluster = require('node:cluster');
const http = require('node:http');
const net = require('node:net');
const fs = require('node:fs');
const { Worker } = require('node:worker_threads');
const { performance, PerformanceObserver } = require('node:perf_hooks');
const v8 = require('node:v8');
const { compute } = require('./compute.cjs');
let gcCount = 0, gcDuration = 0;
const gcObserver = new PerformanceObserver(items => {
  for (const event of items.getEntries()) { gcCount++; gcDuration += event.duration; }
});
gcObserver.observe({ entryTypes: ['gc'] });
function runtimeSnapshot() {
  const memory = process.memoryUsage();
  return { processes: 1, rss_bytes_sum_may_repeat_shared_pages: memory.rss,
    v8_heap_used_bytes: memory.heapUsed, v8_heap_total_bytes: memory.heapTotal,
    external_bytes: memory.external, array_buffer_bytes: memory.arrayBuffers,
    v8_heap_limit_bytes_not_resident_memory: v8.getHeapStatistics().heap_size_limit,
    gc_count: gcCount, gc_duration_ms: gcDuration };
}

function bounded(name, fallback, low = 1, high = 1000000) {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isSafeInteger(value) || value < low || value > high) throw Error(`invalid ${name}`);
  return value;
}
const workers = bounded('BENCH_WORKERS', 1, 1, 4);
if (process.argv[2] === '--batch') {
  const input = JSON.parse(fs.readFileSync(process.argv[3] || 0, 'utf8'));
  const { iterations, seed, repeat } = input;
  if (![iterations, seed, repeat].every(Number.isSafeInteger) || iterations < 1 || iterations > 1000000 || seed < 0 || seed > 1024 || repeat < 0 || repeat > 100000) throw Error('invalid batch input');
  const checksums = [];
  const start = performance.now();
  for (let i = 0; i < repeat; i++) checksums.push(compute(iterations, seed));
  const elapsed = performance.now() - start;
  process.stdout.write(JSON.stringify({ checksums, compute_ms: elapsed }) + '\n');
} else if (cluster.isPrimary && workers > 1) {
  cluster.schedulingPolicy = cluster.SCHED_RR;
  let stopping = false;
  let nextMetrics = 0;
  const metrics = new Map();
  function shutdown(code) {
    if (stopping) return;
    stopping = true;
    process.exitCode = code;
    for (const worker of Object.values(cluster.workers)) worker.kill('SIGTERM');
    setTimeout(() => { for (const worker of Object.values(cluster.workers)) worker.kill('SIGKILL'); }, 6000).unref();
  }
  for (let i = 0; i < workers; i++) {
    const worker = cluster.fork({ BENCH_WORKER_INDEX: String(i) });
    worker.on('message', message => {
      if (message.kind === 'metrics_request') {
        if (metrics.size >= 64) {
          worker.send({ kind: 'metrics_reply', token: message.token, error: 'metrics busy' });
          return;
        }
        const token = ++nextMetrics;
        const entry = { requester: worker, original: message.token, remaining: new Set(Object.keys(cluster.workers)), sums: { active: 0, waiting_timer: 0, waiting_tcp: 0, waiting_cpu: 0, waiting_db: 0, actual_cpu_jobs: 0, runtime: runtimeSnapshot() }, timer: null };
        entry.timer = setTimeout(() => {
          metrics.delete(token);
          if (worker.isConnected()) worker.send({ kind: 'metrics_reply', token: entry.original, error: 'metrics deadline' });
        }, 1000);
        metrics.set(token, entry);
        for (const peer of Object.values(cluster.workers)) peer.send({ kind: 'metrics_collect', token });
      } else if (message.kind === 'metrics_snapshot') {
        const entry = metrics.get(message.token);
        if (!entry || !entry.remaining.delete(String(worker.id))) return;
        for (const key of ['active', 'waiting_timer', 'waiting_tcp', 'waiting_cpu', 'waiting_db', 'actual_cpu_jobs']) entry.sums[key] += message.value[key] || 0;
        for (const [key, value] of Object.entries(message.value.runtime || {})) entry.sums.runtime[key] = (entry.sums.runtime[key] || 0) + value;
        if (entry.remaining.size === 0) {
          clearTimeout(entry.timer);
          metrics.delete(message.token);
          if (entry.requester.isConnected()) entry.requester.send({ kind: 'metrics_reply', token: entry.original, value: entry.sums });
        }
      }
    });
  }
  cluster.on('exit', (worker, code, signal) => {
    if (!stopping) { console.error(`cluster worker exited: ${code ?? signal}`); shutdown(1); }
  });
  process.on('SIGTERM', () => shutdown(0));
  process.on('SIGINT', () => shutdown(0));
} else {
  serve().catch(error => { console.error(error.message); process.exitCode = 1; });
}

async function serve() {
  const { Pool } = require('pg');
  const index = Number(process.env.BENCH_WORKER_INDEX || 0);
  function share(total) { return Math.floor(total / workers) + (index < total % workers ? 1 : 0); }
  const maxActive = share(bounded('BENCH_MAX_ACTIVE', 1024, workers, 65536));
  const maxPending = share(bounded('BENCH_MAX_PENDING', 128, workers, 65536));
  const maxConnections = share(bounded('BENCH_MAX_CONNECTIONS', 4096, workers, 131072));
  const cpuSlots = share(bounded('BENCH_CPU_WORKERS', workers, workers, 4));
  if (cpuSlots !== 1) throw Error('Node requires one CPU worker per HTTP worker');
  const dbSlots = share(bounded('BENCH_DB_POOL', 16, workers, 1024));
  const db = process.env.BENCH_DB_URL ? new Pool({ connectionString: process.env.BENCH_DB_URL, max: dbSlots, connectionTimeoutMillis: 1000, query_timeout: 1000, idleTimeoutMillis: 0 }) : null;
  if (db) db.on('error', error => console.error(`idle PostgreSQL connection: ${error.code || 'error'}`));
  const state = { active: 0, waiting_timer: 0, waiting_tcp: 0, waiting_cpu: 0, waiting_db: 0 };
  let stopped = false;
  const cpu = new Worker(require.resolve('./compute.cjs'));
  let runningCPU = null;
  const cpuQueue = [];
  let taskID = 0;
  let cpuFailure = null;
  let cpuMetricsToken = 0;
  const cpuMetricsWaiters = new Map();
  function dispatchCPU() {
    if (runningCPU || cpuQueue.length === 0 || cpuFailure || stopped) return;
    runningCPU = cpuQueue.shift();
    cpu.postMessage({ task: runningCPU.task, iterations: runningCPU.iterations, seed: runningCPU.seed });
  }
  cpu.on('message', message => {
    if (message.kind === 'metrics') {
      const waiter = cpuMetricsWaiters.get(message.token);
      if (waiter) { cpuMetricsWaiters.delete(message.token); clearTimeout(waiter.timer); waiter.resolve(message.value); }
      return;
    }
    if (!runningCPU || runningCPU.task !== message.task) return;
    const work = runningCPU;
    runningCPU = null;
    state.waiting_cpu--;
    work.resolve(message.checksum);
    dispatchCPU();
  });
  function failCPU(error) {
    cpuFailure = error;
    if (runningCPU) { state.waiting_cpu--; runningCPU.reject(error); runningCPU = null; }
    for (const job of cpuQueue.splice(0)) { state.waiting_cpu--; job.reject(error); }
  }
  cpu.on('error', failCPU);
  cpu.on('exit', code => { if (!stopped) failCPU(Error(`CPU worker exited ${code}`)); });
  function cpuWork(iterations, seed, signal) {
    if (cpuFailure) return Promise.reject(cpuFailure);
    if (cpuQueue.length >= maxPending) return Promise.reject(Object.assign(Error('CPU queue full'), { status: 503 }));
    return new Promise((resolve, reject) => {
      const job = { task: ++taskID, iterations, seed, resolve, reject };
      state.waiting_cpu++;
      cpuQueue.push(job);
      signal.addEventListener('abort', () => {
        const position = cpuQueue.indexOf(job);
        if (position >= 0) { cpuQueue.splice(position, 1); state.waiting_cpu--; reject(Error('cancelled')); }
        // A running bounded CPU job retains its slot until actual completion.
      }, { once: true });
      dispatchCPU();
    });
  }
  const admission = [];
  async function enter(signal) {
    if (state.active < maxActive) { state.active++; return; }
    if (admission.length >= maxPending) throw Object.assign(Error('request queue full'), { status: 503 });
    await new Promise((resolve, reject) => {
      const item = { resolve, reject };
      admission.push(item);
      signal.addEventListener('abort', () => {
        const position = admission.indexOf(item);
        if (position >= 0) { admission.splice(position, 1); reject(Error('cancelled')); }
      }, { once: true });
    });
  }
  function leave() {
    const next = admission.shift();
    if (next) next.resolve(); else state.active--;
  }
  let metricsToken = 0;
  const metricsWaiters = new Map();
  async function localMetrics() {
    const runtime = runtimeSnapshot();
    const worker = await new Promise((resolve, reject) => {
      if (cpuMetricsWaiters.size >= 16) { reject(Error('CPU metrics busy')); return; }
      const token = ++cpuMetricsToken;
      const timer = setTimeout(() => { cpuMetricsWaiters.delete(token); reject(Error('CPU metrics deadline')); }, 500);
      cpuMetricsWaiters.set(token, { resolve, reject, timer });
      cpu.postMessage({ kind: 'metrics', token });
    });
    for (const [key, value] of Object.entries(worker)) runtime[key] = (runtime[key] || 0) + value;
    return { ...state, actual_cpu_jobs: state.waiting_cpu, runtime };
  }
  if (process.send) process.on('message', message => {
    if (message.kind === 'metrics_collect') localMetrics().then(value => {
      if (process.connected) process.send({ kind: 'metrics_snapshot', token: message.token, value });
    }).catch(() => {}); // The primary's existing absolute deadline reports it.
    if (message.kind === 'metrics_reply') {
      const pending = metricsWaiters.get(message.token);
      if (pending) { metricsWaiters.delete(message.token); clearTimeout(pending.timer); message.error ? pending.reject(Error(message.error)) : pending.resolve(message.value); }
    }
  });
  async function metrics() {
    if (!process.send) return localMetrics();
    if (metricsWaiters.size >= 16) throw Object.assign(Error('metrics busy'), { status: 503 });
    return new Promise((resolve, reject) => {
      const token = ++metricsToken;
      const timer = setTimeout(() => { metricsWaiters.delete(token); reject(Error('metrics deadline')); }, 1500);
      metricsWaiters.set(token, { resolve, reject, timer });
      process.send({ kind: 'metrics_request', token });
    });
  }
  function send(response, status, body, contentType = 'text/plain; charset=utf-8') {
    if (response.destroyed || response.writableEnded) return;
    response.writeHead(status, { 'Content-Type': contentType, 'Content-Length': Buffer.byteLength(body) });
    response.end(body);
  }
  async function readBody(request) {
    const chunks = [];
    let length = 0;
    for await (const chunk of request) {
      length += chunk.length;
      if (length > 65536) throw Object.assign(Error('body too large'), { status: 413 });
      chunks.push(chunk);
    }
    return Buffer.concat(chunks, length).toString('utf8');
  }
  const server = http.createServer({ maxHeaderSize: 8192 }, async (request, response) => {
    const abort = new AbortController();
    const timer = setTimeout(() => { abort.abort(); send(response, 504, 'request deadline'); }, 45000);
    response.once('close', () => { if (!response.writableFinished) abort.abort(); });
    let entered = false;
    try {
      const url = new URL(request.url, 'http://localhost');
      if (url.pathname === '/metrics') { send(response, 200, JSON.stringify(await metrics()), 'application/json'); return; }
      await enter(abort.signal);
      entered = true;
      const rid = url.searchParams.get('rid') || '';
      if (!/^\d{1,16}$/.test(rid)) throw Object.assign(Error('invalid rid'), { status: 400 });
      switch (url.pathname) {
        case '/ok': send(response, 200, `ok:${rid}`); break;
        case '/json': {
          if (request.method !== 'POST') throw Object.assign(Error('POST required'), { status: 405 });
          const value = JSON.parse(await readBody(request));
          if (!value || typeof value !== 'object' || Array.isArray(value) || !Number.isSafeInteger(value.counter) || !Object.hasOwn(value, 'data') || !Number.isSafeInteger(value.counter + 1)) throw Object.assign(Error('invalid JSON envelope'), { status: 400 });
          send(response, 200, JSON.stringify({ id: rid, counter: value.counter + 1, data: value.data }), 'application/json');
          break;
        }
        case '/work': {
          const work = url.searchParams.get('work') || '';
          const seed = url.searchParams.get('seed') || '';
          if (!/^x{1,1000}$/.test(work) || !/^s{1,1024}$/.test(seed)) throw Object.assign(Error('invalid CPU input'), { status: 400 });
          send(response, 200, `${rid}:${String(await cpuWork(work.length * 1000, seed.length, abort.signal)).padStart(8, '0')}`);
          break;
        }
        case '/timer':
        case '/wait': {
          state.waiting_timer++;
          try {
            await new Promise((resolve, reject) => {
              const wait = setTimeout(resolve, url.pathname === '/timer' ? 25 : 35000);
              abort.signal.addEventListener('abort', () => { clearTimeout(wait); reject(Error('cancelled')); }, { once: true });
            });
            send(response, 200, `${url.pathname === '/timer' ? 'timer' : 'wait'}:${rid}`);
          } finally { state.waiting_timer--; }
          break;
        }
        case '/tcp': {
          state.waiting_tcp++;
          try {
            await new Promise((resolve, reject) => {
              const socket = net.createConnection({ host: '127.0.0.1', port: bounded('BENCH_PEER_PORT', 1, 1, 65535) });
              let settled = false;
              function finish(error) { if (settled) return; settled = true; socket.destroy(); abort.signal.removeEventListener('abort', cancel); error ? reject(error) : resolve(); }
              function cancel() { finish(Error('cancelled')); }
              abort.signal.addEventListener('abort', cancel, { once: true });
              socket.setTimeout(40000, () => finish(Error('TCP deadline')));
              socket.once('connect', () => socket.write('K'));
              socket.once('data', chunk => finish(chunk.length === 1 && chunk[0] === 75 ? null : Error('incorrect TCP reply')));
              socket.once('error', finish);
              socket.once('end', () => finish(Error('TCP EOF')));
            });
            send(response, 200, `tcp:${rid}`);
          } finally { state.waiting_tcp--; }
          break;
        }
        case '/db': {
          if (!db) throw Error('BENCH_DB_URL is required');
          const key = Number(url.searchParams.get('key'));
          if (!Number.isInteger(key) || key < 1 || key > 1024) throw Object.assign(Error('invalid key'), { status: 400 });
          state.waiting_db++;
          try {
            const result = await db.query('SELECT id,label,score FROM cross_stack_items WHERE id=$1', [key]);
            if (result.rows.length !== 1) throw Error('incorrect database row count');
            const row = result.rows[0];
            send(response, 200, JSON.stringify({ id: row.id, label: row.label, score: row.score, rid }), 'application/json');
          } finally { state.waiting_db--; }
          break;
        }
        default: send(response, 404, 'not found');
      }
    } catch (error) { send(response, error.status || (error instanceof SyntaxError ? 400 : 500), error.message); }
    finally { clearTimeout(timer); if (entered) leave(); }
  });
  server.maxConnections = maxConnections;
  server.headersTimeout = 10000;
  server.requestTimeout = 45000;
  server.keepAliveTimeout = 1000;
  server.timeout = 45000;
  const address = process.env.BENCH_ADDR || '127.0.0.1:8080';
  const split = address.lastIndexOf(':');
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(Number(address.slice(split + 1)), address.slice(0, split), resolve); });
  process.stderr.write(JSON.stringify({ ready: true, stack: 'node', node: process.version, workers, worker_index: index, db_pool_local: dbSlots, cpu_workers_local: cpuSlots, max_active_local: maxActive, max_pending_local: maxPending, max_connections_local: maxConnections }) + '\n');
  async function shutdown() {
    if (stopped) return;
    stopped = true;
    const force = setTimeout(() => server.closeAllConnections(), 5000);
    await new Promise(resolve => server.close(resolve));
    clearTimeout(force);
    failCPU(Error('service stopping'));
    await cpu.terminate();
    if (db) await db.end();
    process.disconnect?.();
  }
  process.on('SIGTERM', shutdown);
  process.on('SIGINT', shutdown);
}
