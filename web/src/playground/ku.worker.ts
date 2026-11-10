import glueUrl from '../generated/ku-web/ku_web.js?url&no-inline';
import wasmUrl from '../generated/ku-web/ku_web_bg.wasm?url&no-inline';
import manifestSource from '../generated/ku-web/engine-manifest.json?raw';
import manifestUrl from '../generated/ku-web/engine-manifest.json?url&no-inline';
import {
  PROTOCOL_VERSION,
  LIMITS,
  byteLength,
  validateInput,
  type EngineInfo,
  type EngineResult,
  type WorkerRequest,
  type WorkerResponse,
} from './protocol';

interface EngineModule {
  default(input: { module_or_path: Uint8Array }): Promise<unknown>;
  engine_info(): string;
  check(request: string): string;
  run(request: string): string;
}
interface EngineManifest {
  version: string;
  buildId: string;
  release: { tag: string; commit: string };
  assets: {
    wasm: { sha256: string; size: number };
    glue: { sha256: string; size: number };
  };
}
const manifest = JSON.parse(manifestSource) as EngineManifest;
async function readBounded(response: Response, limit: number) {
  const reader = response.body?.getReader();
  if (!reader) throw new Error('asset_body');
  const chunks: Uint8Array[] = [];
  let length = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      length += value.length;
      if (length > limit) throw new Error('asset_size');
      chunks.push(value);
    }
  } catch (error) {
    await reader.cancel();
    throw error;
  } finally {
    reader.releaseLock();
  }
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.length;
  }
  return bytes;
}
async function asset(
  url: string,
  expected: { sha256: string; size: number },
  wasm: boolean,
) {
  if (new URL(url, self.location.href).origin !== self.location.origin)
    throw new Error('cross_origin_asset');
  if (
    !Number.isInteger(expected.size) ||
    expected.size <= 0 ||
    expected.size > 32 * 1024 * 1024
  )
    throw new Error('invalid_asset_size');
  const response = await fetch(url, { credentials: 'omit', redirect: 'error' });
  const mime = response.headers.get('content-type')?.split(';')[0].trim();
  if (
    !response.ok ||
    (wasm
      ? mime !== 'application/wasm'
      : !['application/javascript', 'text/javascript'].includes(mime ?? ''))
  )
    throw new Error('asset_http_or_mime');
  const bytes = await readBounded(response, expected.size);
  if (bytes.length !== expected.size) throw new Error('asset_size');
  const digest = Array.from(
    new Uint8Array(await crypto.subtle.digest('SHA-256', bytes)),
    (v) => v.toString(16).padStart(2, '0'),
  ).join('');
  if (digest !== expected.sha256) throw new Error('asset_integrity');
  if (wasm && ![0, 97, 115, 109].every((v, i) => bytes[i] === v))
    throw new Error('invalid_wasm');
  return bytes;
}

function parseResult(raw: string): EngineResult {
  if (typeof raw !== 'string' || byteLength(raw) > 512 * 1024)
    throw new Error('result_size');
  const result = JSON.parse(raw) as EngineResult;
  if (
    ![
      'success',
      'compile_error',
      'runtime_error',
      'budget_exceeded',
      'unsupported_in_browser',
    ].includes(result.status) ||
    typeof result.stdout !== 'string' ||
    typeof result.stderr !== 'string' ||
    !Array.isArray(result.diagnostics)
  )
    throw new Error('result_protocol');
  if (
    byteLength(result.stdout) + byteLength(result.stderr) >
      LIMITS.outputBytes ||
    result.diagnostics.length > LIMITS.diagnostics
  )
    throw new Error('result_limit');
  let diagnosticBytes = 0;
  for (const diagnostic of result.diagnostics) {
    if (
      typeof diagnostic.message !== 'string' ||
      byteLength(diagnostic.message) > 2048
    )
      throw new Error('diagnostic_limit');
    diagnosticBytes += byteLength(diagnostic.message);
  }
  if (diagnosticBytes > LIMITS.diagnosticBytes)
    throw new Error('diagnostic_limit');
  return result;
}

let used = false;
self.onmessage = async (event: MessageEvent<WorkerRequest>) => {
  const request = event.data;
  const identity = {
    protocolVersion: PROTOCOL_VERSION,
    jobId: request.jobId,
    generation: request.generation,
  };
  const send = (message: WorkerResponse) => self.postMessage(message);
  if (used) return;
  used = true;
  let ready = false;
  try {
    if (
      request.protocolVersion !== PROTOCOL_VERSION ||
      !Number.isSafeInteger(request.jobId) ||
      !Number.isSafeInteger(request.generation) ||
      !['run', 'check'].includes(request.action) ||
      typeof request.source !== 'string' ||
      !Array.isArray(request.files)
    )
      throw new Error('request_protocol');
    validateInput(request.source, request.files);
    const started = performance.now();
    const manifestResponse = await fetch(manifestUrl, {
      credentials: 'omit',
      redirect: 'error',
    });
    if (
      !manifestResponse.ok ||
      manifestResponse.headers.get('content-type')?.split(';')[0].trim() !==
        'application/json'
    )
      throw new Error('manifest_http_or_mime');
    const manifestBody = new TextDecoder('utf-8', { fatal: true }).decode(
      await readBounded(manifestResponse, 64 * 1024),
    );
    if (JSON.stringify(JSON.parse(manifestBody)) !== JSON.stringify(manifest))
      throw new Error('manifest_identity');
    const [wasm] = await Promise.all([
      asset(wasmUrl, manifest.assets.wasm, true),
      asset(glueUrl, manifest.assets.glue, false),
    ]);
    const module = (await import(/* @vite-ignore */ glueUrl)) as EngineModule;
    await module.default({ module_or_path: wasm });
    const rawInfo = module.engine_info();
    if (typeof rawInfo !== 'string' || byteLength(rawInfo) > 32 * 1024)
      throw new Error('engine_info_protocol');
    const info = JSON.parse(rawInfo) as EngineInfo;
    if (
      info.engine !== 'wasm-interpreter' ||
      info.protocolVersion !== PROTOCOL_VERSION ||
      info.version !== manifest.version ||
      info.buildId !== manifest.buildId ||
      info.releaseTag !== manifest.release.tag ||
      info.releaseCommit !== manifest.release.commit ||
      !Array.isArray(info.capabilities) ||
      !info.budgets
    )
      throw new Error('engine_identity_mismatch');
    send({
      ...identity,
      type: 'ready',
      info,
      initializationMs: performance.now() - started,
    });
    ready = true;
    const runStarted = performance.now();
    const payload = JSON.stringify({
      source: request.source,
      files: request.files,
    });
    const result = parseResult(module[request.action](payload));
    send({
      ...identity,
      type: 'result',
      result,
      executionMs: performance.now() - runStarted,
    });
  } catch (error) {
    // Traps include panic and stack faults: do not mislabel all of them as OOM.
    const code =
      error instanceof WebAssembly.RuntimeError
        ? 'wasm_trap'
        : ready
          ? 'protocol_error'
          : 'initialization_error';
    send({ ...identity, type: 'failure', code, message: code });
  }
};
