# Ku website and documentation

[中文](README.md)

An Astro + Starlight static site for **0.0.20**. The playground uses a browser preview of the real Ku interpreter. Building requires the browser adapter sources in the full Ku checkout, but no database or execution backend.

## Local development

Use Node.js 22.22.0 or newer. Node.js 24.18.0 has been verified.

```sh
npm ci
npm run dev
```

## Build and upload

```sh
npm run build
```

Run this command in this directory to verify the selected release and build both WASM and the website into `dist/`. Upload all files inside `dist/` to the website root, preserving the `_astro/` assets and `pagefind/` search index directories.

The build uses the confirmed origin `https://ku-lang.com` and allows normal pages to be indexed. Set `SITE_URL` for another origin. Optional checks are `npm run lint` and `npm run check`.

For a noindex local preview, run `npm run build:preview` without production environment variables such as `SITE_URL` or `INDEXABLE`. `npm run preview` only serves the existing output; it does not change its indexing settings. Untranslated fallback pages remain noindex.

## WASM playground and release identity

`/{locale}/playground/` supports all 16 website languages and checks and executes inside a dedicated browser Worker without uploading user source. Each job gets a fresh instance; stop, navigation, page hiding and deadlines terminate the Worker. Native networking, databases, processes, arbitrary disk access and native async scheduling are outside the browser capability contract.

The repository root `rust-toolchain.toml` pins Rust **1.99.1**; `scripts/wasm-toolchain.json` pins `wasm32-unknown-unknown` and wasm-bindgen crate/CLI **0.2.100**. Prepare the tools once without changing the global default toolchain:

```sh
rustup target add --toolchain 1.99.1 wasm32-unknown-unknown
cargo +1.99.1 install --locked --version 0.2.100 --root "$HOME/.cache/ku-web/wasm-bindgen-0.2.100" wasm-bindgen-cli
npm run wasm
```

On Windows use the same Cargo arguments with `.cache/ku-web/wasm-bindgen-0.2.100` below your user directory, or set `KU_WASM_BINDGEN` to the exact CLI executable. `KU_WASM_CACHE` selects the task-specific cache. If dependency downloads need a proxy, set `HTTPS_PROXY`/`HTTP_PROXY` only for the relevant commands.

Build, preview-build and development startup first build the engine. Every build checks GitHub's newest published release, including experimental prereleases, against the selected tag, actual commit and Ku package version in `src/data/versions.json`. A newer release or mismatch fails the build instead of silently reusing an older WASM.

GitHub currently publishes native bundles for `v0.0.20`, with no browser WASM asset. The build uses that exact tag's source plus the reviewed `crates/ku-web/release-adapter.patch` and browser entry sources. Other local interpreter changes are excluded. The engine is explicitly a **browser preview**, not a claimed GitHub-published WASM bundle. The page gets its actual version and build ID from the running `engine_info()`.

The temporary build generates matching WASM/glue/types/manifest, checks the module's actual **128 MiB** memory maximum and runtime identity, and then builds Astro. `dist/playground-build.json` records source identity, artifact hashes and raw/gzip/Brotli sizes. Do not edit `src/generated/ku-web/` or Rust caches manually. Any failed step stops the build.

When updating, verify the new release, update the website version data/package version, and review the adapter against the new tag. Explicitly run `npm run wasm -- --refresh-lock`, review the dependency changes and retain `crates/ku-web/release-Cargo.lock`. Normal builds always use `--locked`. Patch application failures or mismatched runtime identities must be corrected; do not bypass them with old generated assets. `node scripts/build-wasm.mjs --verify-release` performs only the read-only release check.

## Versions and content

- `src/data/versions.json` controls displayed versions, download tags and historical documentation. Package and lockfile versions must match; the build checks this before rendering.
- Current documentation is 0.0.20. Versions 0.0.19, 0.0.18 and 0.0.17 retain their own archived pages and URLs. Do not redirect an old version to current documentation or regenerate it from new content.
- `src/data/archive-pages.json` records preserved historical pages. A missing page fails the build. Register a new archive explicitly; do not regenerate this inventory from an incomplete directory.
- The site supports 16 languages; detailed module references are maintained first in Simplified Chinese and English. Check and run changed examples with the matching Ku release.
- Language changes preserve the document version and topic. Missing translations use the matching English document with an explicit notice. Version changes retain an available matching document or use that version’s documentation entry. Both preserve queries and valid target anchors.
- Installation pages link to the three published platform bundles. Do not advertise unavailable installers or download URLs.
- The WeChat donation image lives at `public/images/wechat-donation.png` and is shared by the Support Ku page and the repository's root Chinese and English READMEs. Keep the original image unchanged. Feedback goes to GitHub Issues or `kagura.gc@qq.com`.

`dist/`, `.astro/` and `node_modules/` are not source files. Install from the lockfile and retain the postinstall versioning patch that protects existing multilingual archives.

## Deployment

The user deploys the site by uploading every file inside `dist/` to the website root, including static assets and the Pagefind index. Do not enable SPA fallback: deep routes use their own `index.html`, and missing paths return404.

Serve `.wasm` as `application/wasm` and Worker/JS as JavaScript; missing engine assets must return real 404s. The local preview already has the appropriate MIME mapping, but production hosting needs its own verification. Hash assets can use long immutable caching; keep HTML/version data refreshable and retain a previous-build asset window. If the host supplies CSP, merge only the necessary `worker-src 'self'` and `script-src 'wasm-unsafe-eval'` permissions. Do not enable general `unsafe-eval`; SharedArrayBuffer/COOP/COEP are unnecessary.

Root language selection, cookies and trusted region integration are described in the [Cloudflare connection guide](deploy/cloudflare/CONNECTION.md). Local preview does not verify live region routing.

Source code uses Apache-2.0; third-party components retain their respective licenses.
