'use strict';
const { parentPort } = require('node:worker_threads');
function compute(iterations, seed) {
  let total = seed;
  for (let i = 0; i < iterations; i++) total = (total * 33 + i + seed) % 100000000;
  return total;
}
module.exports = { compute };
if (parentPort) {
  const { PerformanceObserver } = require('node:perf_hooks');
  let gcCount = 0, gcDuration = 0;
  const observer = new PerformanceObserver(items => {
    for (const event of items.getEntries()) { gcCount++; gcDuration += event.duration; }
  });
  observer.observe({ entryTypes: ['gc'] });
  parentPort.on('message', ({ kind, token, task, iterations, seed }) => {
    if (kind === 'metrics') {
      const memory = process.memoryUsage();
      parentPort.postMessage({ kind: 'metrics', token, value: {
        v8_heap_used_bytes: memory.heapUsed, v8_heap_total_bytes: memory.heapTotal,
        external_bytes: memory.external, array_buffer_bytes: memory.arrayBuffers,
        gc_count: gcCount, gc_duration_ms: gcDuration,
      } });
    } else parentPort.postMessage({ task, checksum: compute(iterations, seed) });
  });
}
