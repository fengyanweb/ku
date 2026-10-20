import { createServer } from 'node:http';
import { createReadStream } from 'node:fs';
import { realpath, stat } from 'node:fs/promises';
import { extname, isAbsolute, join, relative, resolve, sep } from 'node:path';
import { pipeline } from 'node:stream/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { handleRootEntry } from '../src/entry/language.mjs';

const defaultDistDir = fileURLToPath(new URL('../dist/', import.meta.url));
const localOrigin = 'http://127.0.0.1';
const contentTypes = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.map': 'application/json; charset=utf-8',
  '.txt': 'text/plain; charset=utf-8',
  '.xml': 'application/xml; charset=utf-8',
  '.wasm': 'application/wasm',
  '.svg': 'image/svg+xml',
  '.ico': 'image/x-icon',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.gif': 'image/gif',
  '.webp': 'image/webp',
  '.avif': 'image/avif',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.ttf': 'font/ttf',
  '.otf': 'font/otf',
  '.pdf': 'application/pdf',
};

function isWithin(root, candidate) {
  const path = relative(root, candidate);
  return path !== '..' && !path.startsWith(`..${sep}`) && !isAbsolute(path);
}

async function findStaticFile(root, pathname) {
  const decoded = decodeURIComponent(pathname);
  if (
    decoded.includes('\0') ||
    decoded.includes('\\') ||
    decoded.split('/').some((segment) => segment === '..' || segment === '.')
  )
    return null;

  let candidate = resolve(root, `.${decoded}`);
  if (!isWithin(root, candidate)) return null;
  try {
    candidate = await realpath(candidate);
    if (!isWithin(root, candidate)) return null;
    let details = await stat(candidate);
    if (details.isDirectory()) {
      candidate = await realpath(join(candidate, 'index.html'));
      if (!isWithin(root, candidate)) return null;
      details = await stat(candidate);
    }
    return details.isFile() ? { path: candidate, size: details.size } : null;
  } catch (error) {
    if (error.code === 'ENOENT' || error.code === 'ENOTDIR') return null;
    throw error;
  }
}

function sendText(request, response, status, text, headers = {}) {
  response.writeHead(status, {
    'Content-Type': 'text/plain; charset=utf-8',
    'Content-Length': Buffer.byteLength(text),
    ...headers,
  });
  response.end(request.method === 'HEAD' ? undefined : text);
}

async function sendFile(request, response, file, status = 200) {
  response.writeHead(status, {
    'Content-Type':
      contentTypes[extname(file.path).toLowerCase()] ??
      'application/octet-stream',
    'Content-Length': file.size,
    'X-Content-Type-Options': 'nosniff',
  });
  if (request.method === 'HEAD') {
    response.end();
    return;
  }
  // pipeline closes the file stream when a browser aborts a download.
  await pipeline(createReadStream(file.path), response);
}

/**
 * Create an unbound server for local static preview, never a production host.
 * A trusted resolver can be injected by controlled HTTP checks. The CLI has no
 * country override and never treats visitor headers or queries as geolocation.
 *
 * @param {{distDir?: string | URL, resolveCountry?: () => unknown | Promise<unknown>}} [options]
 * @returns {import('node:http').Server}
 */
export function createPreviewServer({
  distDir = defaultDistDir,
  resolveCountry,
} = {}) {
  const directory = resolve(
    distDir instanceof URL ? fileURLToPath(distDir) : distDir,
  );
  return createServer(async (request, response) => {
    try {
      const target = request.url ?? '/';
      if (!target.startsWith('/') || target.startsWith('//')) {
        sendText(request, response, 400, 'Invalid request target.');
        return;
      }
      const url = new URL(target, localOrigin);
      // The shared entry handler only needs these request metadata fields.
      // Keep the original method, including methods the Web Request constructor
      // rejects, so all non-GET/HEAD root requests consistently receive 405.
      const entryResponse = await handleRootEntry(
        {
          url: url.href,
          method: request.method ?? 'GET',
          headers: new Headers({ Cookie: request.headers.cookie ?? '' }),
        },
        { resolveCountry },
      );
      if (entryResponse) {
        response.writeHead(
          entryResponse.status,
          Object.fromEntries(entryResponse.headers),
        );
        response.end();
        return;
      }
      if (request.method !== 'GET' && request.method !== 'HEAD') {
        sendText(request, response, 405, 'Method not allowed.', {
          Allow: 'GET, HEAD',
        });
        return;
      }

      const root = await realpath(directory);
      const file = await findStaticFile(root, url.pathname);
      if (file) {
        await sendFile(request, response, file);
        return;
      }
      const notFound = await findStaticFile(root, '/404.html');
      if (notFound) await sendFile(request, response, notFound, 404);
      else sendText(request, response, 404, 'Not found.');
    } catch (error) {
      if (response.destroyed) return;
      if (response.headersSent) {
        response.destroy();
        return;
      }
      if (error instanceof URIError) {
        sendText(request, response, 400, 'Invalid URL encoding.');
      } else {
        sendText(request, response, 500, 'Local preview could not read dist.');
      }
    }
  });
}

async function main(args) {
  let port = 4321;
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === '--help' || argument === '-h') {
      console.log('Usage: node scripts/preview.mjs [--port 4321]');
      console.log('Local HTTP preview only; binds to 127.0.0.1.');
      return;
    }
    let value;
    if (argument === '--port') value = args[++index];
    else if (argument.startsWith('--port=')) value = argument.slice(7);
    else throw new Error(`Unknown preview option: ${argument}`);
    if (!value || !/^\d+$/.test(value))
      throw new Error('--port must be an integer from 0 to 65535.');
    port = Number(value);
    if (!Number.isSafeInteger(port) || port > 65535)
      throw new Error('--port must be an integer from 0 to 65535.');
  }
  const root = await realpath(defaultDistDir).catch(() => {
    throw new Error('No dist directory. Run npm run build before preview.');
  });
  if (!(await stat(root)).isDirectory())
    throw new Error('The preview dist path must be a directory.');

  const server = createPreviewServer({ distDir: root });
  await new Promise((resolveListen, rejectListen) => {
    server.once('error', rejectListen);
    server.listen(port, '127.0.0.1', () => {
      server.off('error', rejectListen);
      resolveListen();
    });
  });
  const address = server.address();
  console.log(`Ku local preview: http://127.0.0.1:${address.port}/`);
  console.log(
    'Country is unknown locally; ku_lang overrides the English default.',
  );
  console.log('Press Ctrl+C to stop. This server is not for production.');
  let stopping = false;
  const stop = () => {
    if (stopping) return;
    stopping = true;
    server.close();
    server.closeAllConnections();
  };
  process.once('SIGINT', stop);
  process.once('SIGTERM', stop);
  server.once('close', () => {
    process.off('SIGINT', stop);
    process.off('SIGTERM', stop);
  });
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
