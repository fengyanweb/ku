import { execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const npmCli = process.env.npm_execpath;
if (!npmCli) throw new Error('Run this script with npm run build.');

execFileSync(
  process.execPath,
  [npmCli, 'run', 'build:production', '--', ...process.argv.slice(2)],
  {
    cwd: root,
    stdio: 'inherit',
    env: {
      ...process.env,
      SITE_URL: process.env.SITE_URL || 'https://ku-lang.com',
      INDEXABLE: 'true',
    },
  },
);
