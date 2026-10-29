import { execFileSync } from 'node:child_process';
import {
  mkdtempSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { createHash } from 'node:crypto';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const versions = JSON.parse(
  readFileSync(join(root, 'src/data/versions.json'), 'utf8'),
);
if (!/^\d+\.\d+\.\d+$/.test(versions.current)) {
  throw new Error('Expected a numeric website version such as 0.0.19.');
}
const npmCli = process.env.npm_execpath;
if (!npmCli) throw new Error('Run this script with npm run package.');
execFileSync(process.execPath, [npmCli, 'run', 'build:production'], {
  cwd: root,
  stdio: 'inherit',
  env: {
    ...process.env,
    SITE_URL: process.env.SITE_URL || 'https://ku.baiduc.fun',
    INDEXABLE: 'true',
  },
});

const name = `ku-website-v${versions.current}.zip`;
const temporary = mkdtempSync(join(root, '.package-'));
try {
  const archive = join(temporary, name);
  if (process.platform === 'win32') {
    execFileSync(
      'powershell.exe',
      [
        '-NoProfile',
        '-NonInteractive',
        '-Command',
        "Compress-Archive -Path (Join-Path $env:KU_SITE_DIST '*') -DestinationPath $env:KU_SITE_ARCHIVE -CompressionLevel Optimal",
      ],
      {
        stdio: 'inherit',
        env: {
          ...process.env,
          KU_SITE_DIST: join(root, 'dist'),
          KU_SITE_ARCHIVE: archive,
        },
      },
    );
  } else {
    execFileSync('zip', ['-q', '-r', archive, '.'], {
      cwd: join(root, 'dist'),
      stdio: 'inherit',
    });
  }
  const digest = createHash('sha256')
    .update(readFileSync(archive))
    .digest('hex');
  renameSync(archive, join(root, name));
  writeFileSync(join(root, `${name}.sha256`), `${digest}  ${name}\n`);
  console.log(
    `Package: ${join(root, name)}\nSHA256: ${digest}\nNo upload or deployment was performed.`,
  );
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
