import * as fs from "fs";
import { TextDecoder } from "util";

export const MAX_DIAGNOSTIC_SOURCE_BYTES = 1024 * 1024;
export const MAX_DIAGNOSTIC_SOURCE_FILES = 8;
const SOURCE_READ_TIMEOUT_MS = 500;
const MAX_ACTIVE_SOURCE_READS = 4;
let activeSourceReads = 0;

// Ku columns count Unicode scalars; VS Code columns count UTF-16 code units.
// Preserve BOM and clamp coordinates to the actual target line.
export function scalarColumnToUtf16(line: string, column: number): number {
  const wanted = Math.max(0, column - 1);
  let scalar = 0;
  let offset = 0;
  while (offset < line.length && scalar < wanted) {
    const code = line.codePointAt(offset)!;
    offset += code > 0xffff ? 2 : 1;
    scalar++;
  }
  return offset;
}

export function sourceLine(source: string, line: number): string | undefined {
  let start = 0;
  for (let current = 1; current < line; current++) {
    const newline = source.indexOf("\n", start);
    if (newline < 0) return undefined;
    start = newline + 1;
  }
  const end = source.indexOf("\n", start);
  const text = source.slice(start, end < 0 ? source.length : end);
  return text.endsWith("\r") ? text.slice(0, -1) : text;
}

// Timeout stops waiting, not an already-entered filesystem syscall. Late
// completion still closes its handle, and a process-wide cap prevents repeated
// checks from accumulating arbitrarily many stuck filesystem operations.
export async function readDiagnosticSource(file: string): Promise<string | undefined> {
  if (activeSourceReads >= MAX_ACTIVE_SOURCE_READS) return undefined;
  activeSourceReads++;
  let expired = false;
  let timer: NodeJS.Timeout | undefined;
  const reading = (async () => {
    let handle: fs.promises.FileHandle | undefined;
    try {
      handle = await fs.promises.open(file, fs.constants.O_RDONLY | (fs.constants.O_NONBLOCK || 0));
      if (expired) return undefined;
      const before = await handle.stat();
      if (expired || !before.isFile() || before.size > MAX_DIAGNOSTIC_SOURCE_BYTES) return undefined;
      const bytes = Buffer.alloc(before.size + 1);
      let length = 0;
      while (!expired && length < bytes.length) {
        const read = await handle.read(bytes, length, Math.min(65536, bytes.length - length), length);
        if (read.bytesRead === 0) break;
        length += read.bytesRead;
      }
      if (expired || length !== before.size) return undefined;
      const after = await handle.stat();
      if (expired || after.size !== before.size || after.mtimeMs !== before.mtimeMs) return undefined;
      return new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes.subarray(0, length));
    } catch {
      return undefined;
    } finally {
      try { await handle?.close(); } catch { /* The diagnostic remains available without source. */ }
      activeSourceReads--;
    }
  })();
  try {
    return await Promise.race([
      reading,
      new Promise<undefined>((resolve) => {
        timer = setTimeout(() => { expired = true; resolve(undefined); }, SOURCE_READ_TIMEOUT_MS);
      }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}
