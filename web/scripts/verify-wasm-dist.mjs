import { readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { brotliCompressSync, gzipSync } from 'node:zlib';
import { memoryLimits, sha256 } from './wasm-artifacts.mjs';

const web = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const dist = join(web, 'dist');
const generated = join(web, 'src/generated/ku-web');
const manifestBytes = readFileSync(join(generated, 'engine-manifest.json'));
const manifest = JSON.parse(manifestBytes);
const expected = {
  wasm: manifest.assets.wasm.sha256,
  glue: manifest.assets.glue.sha256,
  manifest: sha256(manifestBytes),
};
const matched = {};
const styles = [];
const workers = [];
function visit(directory) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      visit(path);
      continue;
    }
    if (!entry.isFile() || !/\.(?:wasm|js|json|css)$/.test(entry.name))
      continue;
    const bytes = readFileSync(path);
    const url = `/${relative(dist, path).replaceAll('\\', '/')}`;
    const digest = sha256(bytes);
    for (const [key, wanted] of Object.entries(expected)) {
      if (digest === wanted) {
        matched[key] = {
          url,
          sha256: digest,
          size: bytes.length,
          gzipSize: gzipSync(bytes).length,
          brotliSize: brotliCompressSync(bytes).length,
        };
      }
    }
    if (entry.name.endsWith('.css'))
      styles.push({
        url,
        size: bytes.length,
        gzipSize: gzipSync(bytes).length,
        brotliSize: brotliCompressSync(bytes).length,
      });
    if (/ku\.worker[-.].*\.js$/.test(entry.name))
      workers.push({
        url,
        sha256: digest,
        size: bytes.length,
        gzipSize: gzipSync(bytes).length,
        brotliSize: brotliCompressSync(bytes).length,
      });
  }
}
visit(dist);
for (const key of Object.keys(expected)) {
  if (!matched[key])
    throw new Error(`Production output is missing matching ${key}.`);
  if (!matched[key].url.startsWith('/_astro/'))
    throw new Error(`${key} must use the website's hashed Vite assets.`);
}
if (!workers.length)
  throw new Error('The dedicated Ku Worker is missing from output.');
const locales = Object.keys(
  JSON.parse(readFileSync(join(web, 'src/i18n/languages.json'), 'utf8')),
);
for (const lang of locales)
  readFileSync(join(dist, lang, 'playground/index.html'));
const wasm = readFileSync(join(dist, matched.wasm.url));
const memory = memoryLimits(wasm);
if (memory.maximumPages !== 2048)
  throw new Error('Production WASM must enforce a 128 MiB maximum.');
const report = {
  schema: 'ku-playground-build-v1',
  version: manifest.version,
  buildId: manifest.buildId,
  release: manifest.release,
  preview: true,
  locales,
  assets: matched,
  workers,
  styles,
  memory,
};
writeFileSync(
  join(dist, 'playground-build.json'),
  `${JSON.stringify(report, null, 2)}\n`,
);
console.log(
  `Verified static Ku ${manifest.version} WASM/glue/manifest/Worker identities and 128 MiB memory maximum.\nBuild ID: ${manifest.buildId}`,
);
