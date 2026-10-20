import { handleRootEntry } from '../entry/language.mjs';

/** Development-only root routing. Static pages stay prerendered. */
export default function localEntry() {
  return {
    name: 'ku-local-root-entry',
    hooks: {
      'astro:server:setup': ({ server }) => {
        server.middlewares.use(async (request, response, next) => {
          try {
            const target = request.url ?? '/';
            if (!target.startsWith('/') || target.startsWith('//')) {
              next();
              return;
            }
            const url = new URL(target, 'http://127.0.0.1');
            if (url.pathname !== '/') {
              next();
              return;
            }
            // No local query/header override: country is deliberately unknown.
            // The shared handler only consumes these request metadata fields.
            const result = await handleRootEntry({
              url: url.href,
              method: request.method ?? 'GET',
              headers: new Headers({ Cookie: request.headers.cookie ?? '' }),
            });
            if (!result) {
              next();
              return;
            }
            response.writeHead(
              result.status,
              Object.fromEntries(result.headers),
            );
            response.end();
          } catch (error) {
            next(error);
          }
        });
      },
    },
  };
}
