# Optional Cloudflare entry adapter

This is a connection-ready example for an account already using or explicitly
choosing Cloudflare Workers Static Assets. The repository does not establish
that Ku currently uses Cloudflare. No account deployment, hostname binding, DNS
change or paid service has been performed.

`worker.mjs` calls the platform-independent `handleRootEntry` module. It reads
only Cloudflare's inbound `request.cf.country` metadata. Missing metadata or a
lookup failure means unknown and defaults to English; it never trusts a
visitor's `CF-IPCountry`, `X-Forwarded-For` or `X-Real-IP` header. Cloudflare
supplies the country lookup for network traffic, including IPv4 and IPv6; that
platform behavior has not been verified against a live Ku deployment.

Use `wrangler.example.jsonc` as a configuration example to merge with the actual
account's existing configuration. Its paths are relative to this directory.
First build the static website with the confirmed production domain. Keep the
Worker import path, `ASSETS` binding, `run_worker_first: ["/"]` and
`not_found_handling: "404-page"` together. The root must run before its static
`index.html`; ordinary language pages and assets keep static asset handling.
The example intentionally defines no account or public route and disables a
workers.dev hostname. A canonical hostname and the actual account's routing
must be confirmed before deployment.

The root returns 302 for GET/HEAD, an origin-relative fixed language prefix,
the original query string and `Cache-Control: private, no-store`. Other root
methods return 405 with `Allow: GET, HEAD`. No root response is placed in the
Cache API, and automatic redirection does not set cookies. Keep `/` outside
shared-cache overrides in the actual account; inspect existing rules and purge
any older cached root response when connecting the adapter. The local code
does not prove that those account-level rules have been applied.

Non-root requests are passed unchanged to `env.ASSETS.fetch(request)` if the
Worker is invoked for them. Preserve the static site's existing security
headers and 404 page. Do not add a SPA fallback or redirect missing paths to a
language homepage.

No visitor IP, country or per-request query record is persisted by this code.
No geolocation API, MMDB database, account login or separate running Node
service is needed. Platform request/CPU limits and normal hosting costs still
apply. In particular, exhausting a Workers Static Assets free-plan execution
quota can return 429 for a request requiring the Worker; it does not execute
the English fallback. Confirm the actual plan and limits before routing live
traffic. Runtime missing-country fallback and platform unavailability are
different cases.

After connection, verify real HTTP status, Location, cache headers and actual
CN/non-CN metadata, then repeat consecutive root requests with no preference,
`ku_lang=zh-cn` and `ku_lang=en`. Verify deep links, robots, sitemap, static
assets, 404 handling and JavaScript-disabled browsing separately. Local tests
can verify the handler and simulated adapter metadata, not a live CDN cache or
the geolocation of a real visitor.

Official references checked on 2026-10-19:

- [Incoming request metadata](https://developers.cloudflare.com/workers/runtime-apis/request/#incomingrequestcfproperties)
- [Static Assets configuration](https://developers.cloudflare.com/workers/wrangler/configuration/#assets)
- [Static Assets billing and limitations](https://developers.cloudflare.com/workers/static-assets/billing-and-limitations/)
