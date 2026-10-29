# Ku website and documentation

[中文](README.md)

An Astro + Starlight static site for **0.0.19**. The entire `web` directory can be built independently; no parent Ku checkout or database is required.

## Local development

Use Node.js 22.22.0 or newer. Node.js 24.18.0 has been verified.

```sh
npm ci
npm run dev
```

## Check and package

```sh
npm run lint
npm run check
npm run package
```

Packaging rebuilds the site and creates `ku-website-v0.0.19.zip` plus a `.sha256` checksum file. Website files are at the archive root. This command only writes local files; it neither uploads nor deploys. macOS/Linux use the system `zip` tool; Windows uses PowerShell `Compress-Archive`.

The package command defaults to the confirmed origin `https://ku.baiduc.fun`; set `SITE_URL` for another origin. `npm run build` creates a noindex local preview, and `npm run preview` serves it locally.

## Versions and content

- `src/data/versions.json` controls displayed versions, download tags and historical documentation. Package and lockfile versions must match; the build checks this before rendering.
- Archived documentation retains its own content and navigation. Never regenerate historical pages from current documentation.
- The site supports 16 languages; detailed module references are maintained first in Simplified Chinese and English. Check and run changed examples with the matching Ku release.
- Installation pages link to the three published platform bundles. Do not advertise unavailable installers or download URLs.
- The WeChat donation image lives at `public/images/wechat-donation.png` and appears only on the Support Ku page. Feedback goes to GitHub Issues or `kagura.gc@qq.com`.

`dist/`, `.astro/`, `node_modules/` and generated archives are not source files. Install from the lockfile and retain the postinstall versioning patch that protects existing multilingual archives.

## Deployment

This task does not deploy the site. A future deployment must include the complete ZIP, static assets and Pagefind index. Do not enable SPA fallback: deep routes use their own `index.html`, and missing paths return404.

Root language selection, cookies and trusted region integration are described in the [Cloudflare connection guide](deploy/cloudflare/CONNECTION.md). Local preview does not verify live region routing.

Source code uses Apache-2.0; third-party components retain their respective licenses.
