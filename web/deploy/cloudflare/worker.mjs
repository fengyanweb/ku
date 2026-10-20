import { handleRootEntry } from '../../src/entry/language.mjs';

// Optional adapter, not evidence that this site is hosted on Cloudflare. The
// platform supplies request.cf on inbound requests. A visitor-controlled
// CF-IPCountry / X-Forwarded-For / X-Real-IP header is never a country source.
export default {
  async fetch(request, env) {
    const entry = await handleRootEntry(request, {
      resolveCountry: () => request.cf?.country,
    });
    return entry ?? env.ASSETS.fetch(request);
  },
};
