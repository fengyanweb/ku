export const PROTOCOL_VERSION = 1;
export const LIMITS = {
  sourceBytes: 64 * 1024,
  fileCount: 16,
  fileBytes: 256 * 1024,
  outputBytes: 64 * 1024,
  diagnostics: 64,
  diagnosticBytes: 16 * 1024,
  initializationMs: 15_000,
  executionMs: 5_000,
  shareBytes: 64 * 1024,
} as const;

export interface VirtualFile {
  path: string;
  source: string;
}
export interface Diagnostic {
  message: string;
  severity?: 'error' | 'warning' | 'info';
  file?: string;
  offset?: number;
  endOffset?: number;
  line?: number;
  column?: number;
}
export interface EngineInfo {
  version: string;
  buildId: string;
  sourceCommit: string;
  releaseTag: string;
  releaseCommit: string;
  sourceDigest?: string;
  dirty: boolean;
  engine: 'wasm-interpreter';
  protocolVersion: number;
  capabilities: string[];
  budgets: Record<string, number>;
}
export interface EngineResult {
  status:
    | 'success'
    | 'compile_error'
    | 'runtime_error'
    | 'budget_exceeded'
    | 'unsupported_in_browser';
  stdout: string;
  stderr: string;
  diagnostics: Diagnostic[];
}
export interface JobIdentity {
  protocolVersion: number;
  jobId: number;
  generation: number;
}
export interface WorkerRequest extends JobIdentity {
  action: 'run' | 'check';
  source: string;
  files: VirtualFile[];
}
export type WorkerResponse = JobIdentity &
  (
    | { type: 'ready'; info: EngineInfo; initializationMs: number }
    | { type: 'result'; result: EngineResult; executionMs: number }
    | {
        type: 'failure';
        code: 'initialization_error' | 'wasm_trap' | 'protocol_error';
        message: string;
      }
  );
const encoder = new TextEncoder();
export const byteLength = (text: string) => encoder.encode(text).byteLength;

export function validateInput(source: string, files: VirtualFile[]) {
  if (byteLength(source) > LIMITS.sourceBytes) throw new Error('source_limit');
  if (files.length > LIMITS.fileCount) throw new Error('file_limit');
  let total = 0;
  const paths = new Set<string>();
  for (const file of files) {
    if (
      !file ||
      typeof file.path !== 'string' ||
      typeof file.source !== 'string'
    )
      throw new Error('file_path');
    if (
      !/^[\w./-]+\.ku$/.test(file.path) ||
      file.path.startsWith('/') ||
      file.path
        .split('/')
        .some((part) => !part || part === '.' || part === '..') ||
      file.path === 'main.ku' ||
      paths.has(file.path)
    )
      throw new Error('file_path');
    paths.add(file.path);
    total += byteLength(file.source);
  }
  if (total > LIMITS.fileBytes) throw new Error('file_limit');
}

// Ku offsets are UTF-8 bytes; CodeMirror positions are JavaScript UTF-16 units.
// Count complete code points rather than treating a byte offset as a JS index.
export function utf8OffsetToUtf16(source: string, offset: number) {
  let bytes = 0;
  let units = 0;
  for (const point of source) {
    const size = byteLength(point);
    if (bytes + size > Math.max(0, offset)) break;
    bytes += size;
    units += point.length;
  }
  return units;
}

export function diagnosticPosition(source: string, diagnostic: Diagnostic) {
  if (
    typeof diagnostic.offset === 'number' &&
    Number.isFinite(diagnostic.offset)
  )
    return utf8OffsetToUtf16(source, diagnostic.offset);
  const lines = source.split('\n');
  const row = Math.max(
    0,
    Math.min(lines.length - 1, (diagnostic.line ?? 1) - 1),
  );
  let position = lines
    .slice(0, row)
    .reduce((sum, line) => sum + line.length + 1, 0);
  position += Array.from(lines[row])
    .slice(0, Math.max(0, (diagnostic.column ?? 1) - 1))
    .join('').length;
  return Math.min(source.length, position);
}
