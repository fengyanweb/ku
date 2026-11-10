import { createHash } from 'node:crypto';
import { readFileSync, readdirSync } from 'node:fs';
import { join, relative } from 'node:path';

export const sha256 = (bytes) =>
  createHash('sha256').update(bytes).digest('hex');

export function sourceDigest(root) {
  const files = [];
  function visit(directory) {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      if (['.git', 'target', 'node_modules'].includes(entry.name)) continue;
      const path = join(directory, entry.name);
      if (entry.isDirectory()) visit(path);
      else if (entry.isFile()) files.push(path);
      else throw new Error(`Source links are not accepted: ${path}`);
    }
  }
  visit(root);
  const hash = createHash('sha256');
  for (const path of files.sort()) {
    const bytes = readFileSync(path);
    hash.update(relative(root, path).replaceAll('\\', '/'));
    hash.update('\0');
    hash.update(String(bytes.length));
    hash.update('\0');
    hash.update(bytes);
  }
  return hash.digest('hex');
}

// Read the module's actual memory section, rather than constructing a second,
// unused JS Memory that would impose no limit on the interpreter.
export function memoryLimits(bytes) {
  if (
    bytes.length < 8 ||
    !Buffer.from(bytes.subarray(0, 8)).equals(
      Buffer.from([0, 97, 115, 109, 1, 0, 0, 0]),
    )
  )
    throw new Error('Not a WASM 1.0 binary.');
  let offset = 8;
  const unsigned = () => {
    let value = 0;
    let shift = 0;
    for (let i = 0; i < 5; i++) {
      if (offset >= bytes.length) throw new Error('Truncated WASM integer.');
      const byte = bytes[offset++];
      value += (byte & 127) * 2 ** shift;
      if (!(byte & 128)) return value;
      shift += 7;
    }
    throw new Error('Invalid WASM32 integer.');
  };
  while (offset < bytes.length) {
    const section = bytes[offset++];
    const size = unsigned();
    const end = offset + size;
    if (end > bytes.length) throw new Error('Truncated WASM section.');
    if (section === 5) {
      if (unsigned() !== 1)
        throw new Error('Expected exactly one WASM memory.');
      const flags = unsigned();
      if (flags !== 1)
        throw new Error('WASM memory must be bounded, unshared and 32-bit.');
      const initialPages = unsigned();
      const maximumPages = unsigned();
      if (offset !== end) throw new Error('Unexpected WASM memory data.');
      return { initialPages, maximumPages, pageBytes: 65536 };
    }
    offset = end;
  }
  throw new Error('WASM module has no declared memory.');
}
