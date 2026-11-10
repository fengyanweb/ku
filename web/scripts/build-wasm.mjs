import { execFileSync } from 'node:child_process';
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { brotliCompressSync, gzipSync } from 'node:zlib';
import { homedir, tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { memoryLimits, sha256, sourceDigest } from './wasm-artifacts.mjs';

const web = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repository = resolve(web, '..');
const tools = JSON.parse(
  readFileSync(join(web, 'scripts/wasm-toolchain.json'), 'utf8'),
);
const projectToolchain = readFileSync(
  join(repository, 'rust-toolchain.toml'),
  'utf8',
).match(/^channel\s*=\s*"(\d+\.\d+\.\d+)"\s*$/m)?.[1];
if (!projectToolchain)
  throw new Error('The project must pin one explicit stable Rust toolchain.');
tools.rust = projectToolchain;
const versions = JSON.parse(
  readFileSync(join(web, 'src/data/versions.json'), 'utf8'),
);
const generated = join(web, 'src/generated/ku-web');
const stage = mkdtempSync(join(tmpdir(), 'ku-web-release-'));
const git = (args, options = {}) =>
  execFileSync('git', args, { cwd: repository, ...options });
const cargo = (args, options = {}) =>
  execFileSync('cargo', [`+${tools.rust}`, ...args], {
    cwd: stage,
    stdio: 'inherit',
    ...options,
  });
const adapter = join(repository, 'crates/ku-web/release-adapter.patch');
const headers = {
  Accept: 'application/vnd.github+json',
  'User-Agent': 'Ku-website-release-build',
  'X-GitHub-Api-Version': '2022-11-28',
};
const token = process.env.GH_TOKEN || process.env.GITHUB_TOKEN;
if (token) headers.Authorization = `Bearer ${token}`;

async function github(path) {
  const response = await fetch(
    `https://api.github.com/repos/${tools.repository}/${path}`,
    {
      headers,
      signal: AbortSignal.timeout(30000),
    },
  );
  if (!response.ok)
    throw new Error(
      `GitHub release verification failed: HTTP ${response.status}.`,
    );
  const data = await response.text();
  if (Buffer.byteLength(data) > 2097152)
    throw new Error('GitHub metadata exceeds the build limit.');
  return JSON.parse(data);
}

async function verifyRelease() {
  // /releases/latest excludes prereleases. Ku's current experimental release
  // must be selected from actual published releases instead.
  const releases = await github('releases?per_page=100');
  if (!Array.isArray(releases)) throw new Error('Invalid GitHub release list.');
  const release = releases
    .filter((item) => !item.draft && item.published_at)
    .sort((a, b) => Date.parse(b.published_at) - Date.parse(a.published_at))[0];
  if (!release) throw new Error('No published Ku release exists.');
  if (
    release.tag_name !== versions.releaseTag ||
    versions.current !== versions.languageVersion ||
    versions.current !== versions.released
  )
    throw new Error(
      `GitHub now publishes ${release.tag_name}; website selects ${versions.releaseTag}. Update the version data and browser adapter before building. An older engine will not be reused.`,
    );
  const ref = await github(
    `git/ref/tags/${encodeURIComponent(release.tag_name)}`,
  );
  let object = ref.object;
  for (let depth = 0; object?.type === 'tag' && depth < 4; depth++) {
    object = (await github(`git/tags/${object.sha}`)).object;
  }
  if (
    object?.type !== 'commit' ||
    !/^[a-f0-9]{40}$/.test(object.sha) ||
    object.sha !== versions.releaseCommit
  )
    throw new Error(
      'Published release source commit differs from the selected website source.',
    );
  return {
    id: release.id,
    tag: release.tag_name,
    commit: object.sha,
    prerelease: Boolean(release.prerelease),
    url: release.html_url,
    publishedAt: release.published_at,
  };
}

async function extractRelease(commit) {
  const archive = join(stage, 'source.tar');
  let local = false;
  try {
    git(['cat-file', '-e', `${commit}^{commit}`], { stdio: 'ignore' });
    local = true;
  } catch {
    // A fresh website checkout may not have fetched this tag yet.
  }
  if (local) {
    git(['archive', '--format=tar', `--output=${archive}`, commit]);
    execFileSync('tar', ['-xf', archive, '-C', stage]);
  } else {
    const response = await fetch(
      `https://codeload.github.com/${tools.repository}/tar.gz/${commit}`,
      { signal: AbortSignal.timeout(60000) },
    );
    if (!response.ok)
      throw new Error(`Source archive: HTTP ${response.status}.`);
    const chunks = [];
    let size = 0;
    for await (const chunk of response.body) {
      size += chunk.length;
      if (size > 67108864) throw new Error('Source archive exceeds 64 MiB.');
      chunks.push(chunk);
    }
    writeFileSync(archive, Buffer.concat(chunks));
    execFileSync('tar', ['-xzf', archive, '--strip-components=1', '-C', stage]);
  }
  rmSync(archive);
  const manifest = readFileSync(join(stage, 'Cargo.toml'), 'utf8');
  const version = manifest.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  if (version !== versions.current)
    throw new Error(
      `Release Cargo package version ${version} does not match ${versions.current}.`,
    );
  return version;
}

function overlayAdapter() {
  if (!existsSync(adapter))
    throw new Error('The reviewed browser adapter patch is missing.');
  const patch = readFileSync(adapter);
  if (!patch.length) throw new Error('The browser adapter patch is empty.');
  execFileSync('git', ['apply', '--check', adapter], {
    cwd: stage,
    stdio: 'inherit',
  });
  execFileSync('git', ['apply', adapter], { cwd: stage, stdio: 'inherit' });
  for (const path of ['src/browser', 'crates/ku-web']) {
    const source = join(repository, path);
    if (!existsSync(source))
      throw new Error(`Browser adapter source missing: ${path}`);
    cpSync(source, join(stage, path), {
      recursive: true,
      filter: (name) =>
        !/(?:^|[/\\])(?:target|pkg|\.git)(?:[/\\]|$)/.test(name),
    });
  }
  // The reviewed patch extends the release's existing build identity script.
  // Never replace that script with the older browser-only root build.
  const releaseBuild = join(stage, 'build.rs');
  if (
    !existsSync(releaseBuild) ||
    !readFileSync(releaseBuild, 'utf8').includes(
      'browser_import_support::generate(&root, &out)',
    )
  )
    throw new Error(
      'The reviewed browser adapter must extend the release build script while preserving its build identity.',
    );
  return sha256(patch);
}

function bindgenBinary(cache) {
  const bin = join(
    cache,
    `wasm-bindgen-${tools.wasmBindgen}`,
    'bin',
    process.platform === 'win32' ? 'wasm-bindgen.exe' : 'wasm-bindgen',
  );
  const selected = process.env.KU_WASM_BINDGEN || bin;
  if (!existsSync(selected))
    throw new Error(
      `Install the pinned glue tool first: cargo +${tools.rust} install --locked --version ${tools.wasmBindgen} --root "${dirname(dirname(bin))}" wasm-bindgen-cli`,
    );
  const actual = execFileSync(selected, ['--version'], {
    encoding: 'utf8',
  }).trim();
  if (actual !== `wasm-bindgen ${tools.wasmBindgen}`)
    throw new Error(
      `Wrong wasm-bindgen CLI: ${actual}. Expected ${tools.wasmBindgen}.`,
    );
  return selected;
}

try {
  const release = await verifyRelease();
  if (process.argv.includes('--verify-release')) {
    console.log(JSON.stringify(release, null, 2));
  } else {
    const version = await extractRelease(release.commit);
    const overlaySha256 = overlayAdapter();
    const releaseLock = join(repository, 'crates/ku-web/release-Cargo.lock');
    if (process.argv.includes('--refresh-lock')) {
      // Resolve against the original release lock. Cargo extends it for the
      // browser target rather than replacing all native dependency versions.
      cargo(
        [
          'metadata',
          '--manifest-path',
          join(stage, 'Cargo.toml'),
          '--format-version',
          '1',
          '--filter-platform',
          tools.target,
        ],
        {
          stdio: ['ignore', 'ignore', 'inherit'],
        },
      );
      cpSync(join(stage, 'Cargo.lock'), releaseLock);
      console.log(`Updated reviewed browser dependency lock: ${releaseLock}`);
      process.exitCode = 0;
    } else {
      if (!existsSync(releaseLock))
        throw new Error(
          'Missing browser release lock. Run npm run wasm -- --refresh-lock and review its dependency changes.',
        );
      cpSync(releaseLock, join(stage, 'Cargo.lock'));
      const sourceSha256 = sourceDigest(stage);
      const buildId = sha256(
        JSON.stringify({ release, sourceSha256, overlaySha256, tools }),
      );
      const cache =
        process.env.KU_WASM_CACHE || join(homedir(), '.cache/ku-web');
      mkdirSync(cache, { recursive: true });
      const bindgen = bindgenBinary(cache);
      const output = join(stage, 'generated');
      const target = join(cache, 'target', `rust-${tools.rust}`);
      const rustflags = [
        '-C',
        `link-arg=--max-memory=${tools.maximumMemoryBytes}`,
        '-C',
        `link-arg=-zstack-size=${tools.stackBytes}`,
      ].join(' ');
      const env = {
        ...process.env,
        CARGO_TARGET_DIR: target,
        RUSTFLAGS: rustflags,
        KU_WEB_RELEASE_TAG: release.tag,
        KU_WEB_RELEASE_COMMIT: release.commit,
        KU_WEB_SOURCE_SHA256: sourceSha256,
        KU_WEB_OVERLAY_SHA256: overlaySha256,
        KU_WEB_BUILD_ID: buildId,
        KU_WEB_SOURCE_DIR: stage,
        KU_WEB_DIRTY: 'true',
      };
      cargo(
        [
          'build',
          '--manifest-path',
          join(stage, 'Cargo.toml'),
          '--locked',
          '--release',
          '--target',
          tools.target,
          '-p',
          'ku-web',
        ],
        { env },
      );
      execFileSync(
        bindgen,
        [
          join(target, tools.target, 'release/ku_web.wasm'),
          '--target',
          'web',
          '--out-name',
          'ku_web',
          '--out-dir',
          output,
        ],
        {
          cwd: stage,
          stdio: 'inherit',
        },
      );
      writeFileSync(join(output, 'package.json'), '{"type":"module"}\n');
      const wasm = readFileSync(join(output, 'ku_web_bg.wasm'));
      const memory = memoryLimits(wasm);
      if (memory.maximumPages * memory.pageBytes !== tools.maximumMemoryBytes)
        throw new Error(
          'The compiled WASM memory does not enforce the selected maximum.',
        );
      const glue = await import(pathToFileURL(join(output, 'ku_web.js')).href);
      await glue.default({ module_or_path: wasm });
      const info = JSON.parse(glue.engine_info());
      for (const [key, expected] of Object.entries({
        version,
        buildId,
        releaseTag: release.tag,
        releaseCommit: release.commit,
      })) {
        if (info[key] !== expected)
          throw new Error(
            `The real engine reports mismatched ${key}: ${info[key]}.`,
          );
      }
      if (info.engine !== 'wasm-interpreter' || info.protocolVersion !== 1)
        throw new Error('Unsupported interpreter engine/protocol.');
      const assets = {};
      for (const [key, name] of [
        ['wasm', 'ku_web_bg.wasm'],
        ['glue', 'ku_web.js'],
      ]) {
        const bytes = readFileSync(join(output, name));
        assets[key] = {
          sha256: sha256(bytes),
          size: bytes.length,
          gzipSize: gzipSync(bytes).length,
          brotliSize: brotliCompressSync(bytes).length,
        };
      }
      const manifest = {
        protocolVersion: 1,
        version,
        buildId,
        sourceCommit: release.commit,
        releaseTag: release.tag,
        releaseCommit: release.commit,
        dirty: true,
        sourceSha256,
        overlaySha256,
        engine: 'wasm-interpreter',
        preview: true,
        released: false,
        release,
        tools,
        memory,
        assets,
      };
      writeFileSync(
        join(output, 'engine-manifest.json'),
        `${JSON.stringify(manifest, null, 2)}\n`,
      );
      mkdirSync(dirname(generated), { recursive: true });
      // Stage on the destination filesystem so rename also works when the OS
      // temporary directory and checkout are on different volumes.
      const next = `${generated}.next-${process.pid}`;
      const previous = `${generated}.previous-${process.pid}`;
      try {
        cpSync(output, next, { recursive: true });
        if (existsSync(generated)) renameSync(generated, previous);
        try {
          renameSync(next, generated);
        } catch (error) {
          if (existsSync(previous)) renameSync(previous, generated);
          throw error;
        }
      } finally {
        rmSync(next, { recursive: true, force: true });
        rmSync(previous, { recursive: true, force: true });
      }
      console.log(
        `Built real Ku ${version} (${release.tag}, ${release.commit}) browser preview.\nBuild ID: ${buildId}\nWASM: ${assets.wasm.size} bytes; gzip ${assets.wasm.gzipSize}; brotli ${assets.wasm.brotliSize}.`,
      );
    }
  }
} finally {
  rmSync(stage, { recursive: true, force: true });
}
