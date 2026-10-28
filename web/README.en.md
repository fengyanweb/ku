# Ku website and documentation

[中文](README.md) · [English](README.en.md)

This is the Astro + Starlight static project in `ku/web`; the original `docs-site`
is retained. The deployed site needs no persistent Node service, business backend
or database.

The 0.0.19 work updates source and produces a local static package only. It does
not upload or deploy the website. The dated entry-integration and 0.0.18 closeout
notes below are historical records, not evidence of the current public site.

## Run and build

Use Node 24.18.0 (minimum 22.22.0) and npm. From this directory:

```sh
npm ci
npm run dev
```

```sh
npm run lint
npm run check
npm run build
npm run preview
```

The search index is generated at build time; check search through preview. Preview
is a local loopback static server that reuses the production entry decision logic
to check real HTTP 302 responses. It does not introduce a production Node server.
The terminal prints its actual URL, normally `http://127.0.0.1:4321`; change the
port with `npm run preview -- --port 4322`. Stop it with Ctrl+C. `dist/` is the final
static output and `.astro/` is rebuildable cache; neither is committed.

## Languages

`src/i18n/languages.json` is the sole locale-list configuration. It contains 16
languages: German, Greek, English, Spanish, Persian, French, Italian, Japanese,
Korean, Polish, Brazilian Portuguese, Romanian, Russian, Turkish, Simplified
Chinese and Traditional Chinese.

- Homepages use `/{locale}/`. Every language retains core documentation; English
  and Simplified Chinese also have the complete module references and tool/example
  entries. New release boundaries are complete in those two languages; English
  supplements in other languages are labeled rather than presented as translations.
- Content lives in `src/content/docs/{locale}/`, UI strings in
  `src/i18n/{locale}.json`, and additional Starlight strings in
  `src/content/i18n/{lang}.json`. The latter uses BCP-47 names such as `zh-CN`,
  `zh-TW` and `pt-BR`, distinct from route locales.
- Persian is `fa` / `فارسی` and uses RTL. Code, terminal text and API identifiers
  remain LTR.
- Build checks dictionary keys, arrays and route structure. A missing dictionary
  fails instead of silently passing an English fallback off as a full translation.
- Language switches preserve topic and documentation version. Canonical URLs and
  hreflang retain language, version and topic; historical pages do not canonicalize
  to current content.
- When changing APIs, check `docs/syntax.md`, `docs/semantics.md`,
  `src/stdlib/metadata.rs` and the implementation before updating translations.
  Do not translate code or CLI commands.

## Versioned documentation

The site uses the community [starlight-versions](https://starlight-versions.vercel.app/getting-started/)
plugin listed in the official plugin directory, pinned to 0.10.1. It is marked
early development. Its multilingual, search, layout and archive integration was
checked for this project; updates are not adopted automatically.

| Content                    | Example path                       | Maintenance                             |
| -------------------------- | ---------------------------------- | --------------------------------------- |
| Current documentation      | `/en/docs/guide/language/`         | Edit `src/content/docs/en/docs/…`       |
| Fixed 0.0.17 documentation | `/en/v0.0.17/`                     | Based on the actual v0.0.17 tag and CLI |
| Fixed 0.0.18 snapshot      | `/en/v0.0.18/docs/guide/language/` | Retained independently of current edits |
| Current release history    | `/en/releases/`                    | Updated for actual releases             |
| Historical release history | `/en/v0.0.18/releases/`            | Frozen with that documentation version  |

`src/data/versions.json` centrally defines current and archived versions. The
website package, current documentation and download target are 0.0.19, with the
planned tag `v0.0.19`. Current documentation keeps its unversioned URLs. Existing
v0.0.18 and v0.0.17 snapshots remain independent. Actual publication, assets and
SHA256SUMS are determined by the corresponding GitHub Release; version settings or
a local build do not prove publication or deployment.

Historical note: the 2026-10-21 downloads used the `v0.0.18-r1` revision tag with
package version 0.0.18; the `v0.0.18/` snapshot corresponds to the original tag.
Those records remain historical and are not current 0.0.19 download entries.

For a future 0.1 or 1.0 documentation archive:

1. Complete and review current content and compatibility notes in all languages.
2. Keep every existing `archives` entry in versions.json and add one version at a
   time, for example `{ "slug": "v0.1.0", "label": "0.1.0" }`.
3. Run `npm run build`. On first encountering a new version, the plugin copies
   content, internal links, sidebar and required resources, then creates
   `src/content/versions/{slug}.json`. Do not add the archive entry before its
   content is ready: both dev and build can create snapshots.
4. Commit the generated `src/content/docs/{locale}/{slug}/`,
   `src/content/versions/{slug}.json`, root
   `src/content/docs/{slug}/.gitkeep` and required resources. They are permanent
   historical material.
5. Set `current` to the next development version and edit unversioned content.
   Do not regenerate old archives, remove their entries or redirect them home.

`src/integrations/versioning.mjs` preserves/restores root archive markers before
the plugin detects snapshots. A multilingual-only archive leaves an empty root
version directory, which Git does not retain; without the marker, upstream can
mistake an existing snapshot for a new one. The marker records existence only and
does not rewrite old text. Historical directories are excluded from routine
formatting. The v0.0.18 development snapshot was synchronized at its original
finalization; 0.0.19 uses current documentation without rewriting published history.

Current Ku source uses Apache-2.0; see repository-root `LICENSE` and `NOTICE`.
Third-party components retain their own licenses. Old licensing statements in
historical versions describe their state at that time.

The `postinstall` script `scripts/patch-versioning.mjs` fixes the reproduced 0.10.1
problem of copying older language snapshots into a new version. It changes one
directory-exclusion condition, checks dependency version/source text, and can run
repeatedly. Unexpected versions or source fail installation. Review the patch
when upgrading and remove it after adopting an upstream fix. Do not bypass it
with `npm ci --ignore-scripts`.

Version switches and previous/next navigation preserve topic. Documentation search
filters by version; homepage search covers current content only. Historical pages
display their status and a path back to current content. Release records are also
archived so old pages do not present later release facts.

## Indexing and deployment

Without a confirmed production URL, pages use `noindex, nofollow`, robots disallow
crawling, and no invented canonical URL is emitted. A production build requires
an explicitly confirmed HTTPS `SITE_URL` and `INDEXABLE=true`, then:

```sh
npm run build:production
```

This prepares output; it does not authorize or perform deployment. Production
builds generate per-language/version canonical URLs, real-translation hreflang,
WebSite JSON-LD, robots and sitemap. Preview and production use separate builds.
Skipping sitemap when no domain is supplied is expected.

The old fixed Chinese root 301 and language-choice gateway page were removed.
Handle `/` at the request entry; do not configure `/ → /zh-cn/` as a 301/308. Do
not enable an SPA fallback: deep routes use their own index.html and missing paths
must return real 404s. Uploading dist to a static host without request handling
does not provide real IP-country routing.

The actual canonical domain, host, cache policy and regional access quality need
live acceptance. Revalidate HTML; hashed assets may be cached long-term. Roll back
the complete HTML, resources and Pagefind index from one build together.

Direct dependencies are pinned and package-lock.json is committed. Keep the
targeted postcss-selector-parser 7.1.6 override for the known old dependency-chain
issue; do not use `npm audit fix --force` to downgrade frameworks indiscriminately.

Maintain release scope, APIs and limits in `src/content/docs/zh-cn/` and
`src/content/docs/en/`. Local historical review reports stay separate and do not
prove public deployment of this website source.

## Automatic root-language selection: 2026-10-19 record

At this handoff, no canonical domain or hosting configuration had been confirmed
in the repository. Delivery included platform-independent logic and an
**undeployed** Cloudflare Workers Static Assets example; it did not migrate hosts,
buy a plan or change DNS. These notes do not establish today's public settings.

`src/entry/language.mjs` is the single source for entry and Cookie rules:

1. Only GET/HEAD requests to the unlocalized root `/` are considered. Other root
   methods return 405; other paths use normal static routing.
2. A single valid `ku_lang=zh-cn` or `ku_lang=en` takes priority, without consulting
   a country source.
3. Without a valid preference, a trusted country code CN selects Simplified
   Chinese. Other codes, including HK/MO/TW, missing values or lookup failure select
   English. Browser language, timezone, URL country parameters and visitor-supplied
   country/proxy headers are not used.
4. Return 302 to a fixed local language prefix, preserve query, and set
   `Cache-Control: private, no-store`. Automatic redirects do not write a Cookie
   or retain IP/country data.

Homepages and documentation share `LanguageSwitch` through Starlight's
LanguageSelect interface. All 16 languages remain crawlable, keyboard-accessible
ordinary links. Only an explicit normal left-click/Enter activation writes a
preference; modifier-key navigation does not alter the current browser preference.
Simplified Chinese/English write a one-year host-only Cookie with Path=/,
SameSite=Lax and Secure on HTTPS. Other languages only clear the old preference
under the documented whitelist. Ordinary content links, logos, prefetch and initial
rendering do not save a language choice.

`src/i18n/routes.ts` derives translation/anchor mappings from actual content and
rendered headings, preserving topic and version without relying on framework
missing-translation fallback URLs. A missing translation links to the target
homepage's real explanatory anchor with a target-language notice. With JavaScript,
query is preserved and the old hash is kept only if the destination contains that
ID. Without JavaScript, links still work, but static links cannot dynamically add
the current query or validate its hash. Document metadata and sitemap include real
translations only. Framework fallback pages are noindex and excluded from sitemap.

Local dev/preview treat country as unknown, so `/` defaults to English. Use the
menu to write a real preference Cookie, then reopen `/`. There is no visitor-facing
query/header country override. Controlled tests inject trusted metadata for CN,
IPv4/IPv6 and other boundaries; this is not real public-location acceptance.

`src/pages/index.astro` is a no-gateway fallback for incorrectly connected static
hosting. JavaScript reads the same Cookie and redirects immediately; without
JavaScript, meta refresh selects English. It is neither a server 302 nor country
detection. Proper entry integration intercepts the root before this file. URI
fragments never reach the server; only query is preserved there. Browser fragment
inheritance needs separate verification and is not server-side recognition.

### Trusted country sources and cache

See [Cloudflare integration](deploy/cloudflare/CONNECTION.md) and
`wrangler.example.jsonc`. Configure `run_worker_first: ["/"]` only for the root;
otherwise asset-first routing returns static root HTML first. Content/resources
remain static. The Worker reads only the platform's `request.cf.country`, not
CF-IPCountry, XFF or X-Real-IP. No offline MMDB is used, so this design adds no
database download/update job, binary geolocation dataset or DB-IP attribution.
If an existing Nginx gateway is later confirmed, integrate the same policy with
its actual trusted proxy chain rather than applying an unverified production
snippet.

The real CDN must bypass shared caching for **exact root URI /** and purge old
root 301/200 responses. Do not rely solely on Vary: Cookie or disable caching
across the whole site whenever ku_lang exists. Passing code checks does not prove
CDN rules took effect without account/configuration evidence.

The design calls no paid location API and adds no business backend; hosting is
not thereby guaranteed free. The historical note recorded Workers Free limits of
100,000 requests/day and 10 ms CPU/request. Worker-first Static Assets root
requests can return 429 when over quota rather than automatically falling back to
English. Before deployment, check the actual account plan and current official
rules: [country information](https://developers.cloudflare.com/network/ip-geolocation/),
[Worker static routing](https://developers.cloudflare.com/workers/static-assets/routing/worker-script/),
and [quotas/over-limit behavior](https://developers.cloudflare.com/workers/static-assets/billing-and-limitations/).

The outstanding deployment inputs at that handoff were the canonical domain and
www/bare-domain policy, real host/gateway configuration, deployment and root-cache
permissions, plus the trusted proxy chain and any origin bypass if country arrives
through an origin header. Verify live entry behavior on the actual host under the
conditions above. Historical local reports do not replace public release evidence.

## v0.0.18 release closeout: 2026-10-20 record

Simplified Chinese/English and the existing v0.0.18 snapshot were synchronized
with release scope, install targets, the previous JSON default, process limits and
follow-up plans. Other languages retained existing content, with locally labeled
English additions and links to full references rather than invented translations.

The historical HTTP issue was recorded as: it occurred previously, this round's
short tests did not reproduce it, the root cause was unconfirmed, and formal
30-minute acceptance had not passed. Maintainers retained it as a known issue
rather than a separate release blocker. Full M:N, general network scheduling and
the complete database/platform matrix moved to later work; both paused items
remained paused.

`ku supervise` requires delegated cgroup v2 controls on Linux or permitted Job
Objects on Windows. Windows limits committed memory; macOS rejects this isolation
mode natively. The coordinator has no OS hard limit and the whole heap of a single
Task is not precisely metered. Do not claim an absolute memory ceiling for the
whole system. See the language-specific support pages and
`docs/process-execution.md`.

That website closeout delivered source and build checks only; upload progress,
preview and local builds were not declared completed public deployment.

## Documentation organization and example checks

Current navigation groups getting started, language/tools, standard library,
examples and project information. The standard library has its own index. HTTP,
time, files, databases and other modules separately document parameters, return
values, errors, lifetime and backend requirements. The organization draws on
[Go Documentation](https://go.dev/doc/) and the
[Rust Standard Library](https://doc.rust-lang.org/std/).

0.0.17 content comes from its real tag, not an automatic copy of current pages.
The release page lists original 0.0.1–0.0.16 development records without inventing
separate Releases. The version selector links only to existing matching topics,
otherwise the corresponding version entry. Missing localized historical content
explicitly enters English.

For new/changed Ku examples, use the released CLI's `check`, actually `run`
self-contained programs, and build/run examples advertised as native. HTTP uses
loopback, bounded waits and port cleanup. Database examples requiring real services
state their dependencies separately: static checking/compilation does not establish
a successful query. Put temporary scripts, inputs and build copies outside the
project and remove them afterward. Do not put internal work plans or AI prompts
in user reference pages.
