import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightVersions from './src/integrations/versioning.mjs';
import localEntry from './src/integrations/local-entry.mjs';
import sitemap from '@astrojs/sitemap';
import { existsSync, readFileSync } from 'node:fs';

const kuGrammar = JSON.parse(
  readFileSync(
    new URL('./src/data/ku.tmLanguage.json', import.meta.url),
    'utf8',
  ),
);
kuGrammar.name = 'ku';
kuGrammar.aliases = ['Ku'];
const readJson = (path) =>
  JSON.parse(readFileSync(new URL(path, import.meta.url), 'utf8'));
const languages = readJson('./src/i18n/languages.json');
const versioning = readJson('./src/data/versions.json');
const archivePages = readJson('./src/data/archive-pages.json');
const packageMetadata = readJson('./package.json');
const lockMetadata = readJson('./package-lock.json');
if (
  versioning.current !== versioning.released ||
  versioning.current !== versioning.languageVersion ||
  versioning.current !== packageMetadata.version ||
  versioning.current !== lockMetadata.version ||
  versioning.current !== lockMetadata.packages?.['']?.version
)
  throw new Error(
    'Website package, lockfile and current release versions must agree.',
  );
// Each archived version has its own source and URL. Never redirect it to
// current documentation or generate its contents from the current release.
for (const archive of versioning.archives) {
  if (
    !existsSync(
      new URL(`./src/content/versions/${archive.slug}.json`, import.meta.url),
    )
  )
    throw new Error(`Missing preserved documentation archive: ${archive.slug}`);
  const pages = archivePages[archive.slug];
  if (!Array.isArray(pages) || pages.length === 0)
    throw new Error(`Missing archived page inventory: ${archive.slug}`);
  for (const page of pages) {
    if (
      !page.split('/').includes(archive.slug) ||
      !existsSync(new URL(`./src/content/docs/${page}`, import.meta.url))
    )
      throw new Error(`Missing archived page: ${page}`);
  }
}
const translations = Object.fromEntries(
  Object.keys(languages).map((locale) => [
    locale,
    readJson(`./src/i18n/${locale}.json`),
  ]),
);
function validateTranslation(source, translated, path) {
  if (typeof source !== typeof translated || translated === null)
    throw new Error(`Invalid translation: ${path}`);
  if (Array.isArray(source)) {
    if (!Array.isArray(translated) || translated.length !== source.length)
      throw new Error(`Invalid translation array: ${path}`);
    source.forEach((item, index) =>
      validateTranslation(item, translated[index], `${path}[${index}]`),
    );
  } else if (typeof source === 'object') {
    if (
      JSON.stringify(Object.keys(source).sort()) !==
      JSON.stringify(Object.keys(translated).sort())
    )
      throw new Error(`Translation keys differ: ${path}`);
    for (const key of Object.keys(source))
      validateTranslation(source[key], translated[key], `${path}.${key}`);
  } else if (
    typeof translated === 'string' &&
    (!translated.trim() ||
      (source.startsWith('docs/') && source !== translated))
  ) {
    throw new Error(`Missing translation or changed route: ${path}`);
  }
}
for (const [locale, dictionary] of Object.entries(translations))
  validateTranslation(translations.en, dictionary, locale);
const preferenceMessages = readJson('./src/i18n/language-preferences.json');
if (
  JSON.stringify(Object.keys(preferenceMessages).sort()) !==
  JSON.stringify(Object.keys(languages).sort())
)
  throw new Error(
    'Language preference notices must cover every supported locale.',
  );
for (const [locale, dictionary] of Object.entries(preferenceMessages))
  validateTranslation(
    preferenceMessages.en,
    dictionary,
    `preference.${locale}`,
  );
const searchTranslations = Object.fromEntries(
  Object.keys(languages).map((locale) => [
    locale,
    readJson(`./src/content/i18n/${languages[locale].lang}.json`),
  ]),
);
const placeholders = (value) =>
  (
    value.match(
      /\{\{(?:link|label|slug|title)\}\}|\[(?:COUNT|SEARCH_TERM)\]/g,
    ) ?? []
  )
    .sort()
    .join(',');
for (const [locale, dictionary] of Object.entries(searchTranslations)) {
  validateTranslation(searchTranslations.en, dictionary, `search.${locale}`);
  for (const [key, value] of Object.entries(dictionary)) {
    if (placeholders(value) !== placeholders(searchTranslations.en[key]))
      throw new Error(`Translation placeholders differ: ${locale}.${key}`);
  }
}
const groupTranslations = (key) =>
  Object.fromEntries(
    Object.entries(translations).map(([locale, strings]) => [
      locale,
      strings[key],
    ]),
  );
const site = process.env.SITE_URL;
if (site && (!URL.canParse(site) || new URL(site).protocol !== 'https:')) {
  throw new Error('SITE_URL must be an absolute HTTPS URL.');
}
if (process.env.INDEXABLE === 'true' && !site) {
  throw new Error('INDEXABLE=true requires the confirmed SITE_URL.');
}

export default defineConfig({
  site,
  output: 'static',
  trailingSlash: 'always',
  integrations: [
    localEntry(),
    sitemap({
      filter: (url) => {
        const path = new URL(url).pathname;
        if (!(path.split('/')[1] in languages)) return false;
        // Sitemap runs after HTML generation. Exclude Starlight's synthetic
        // untranslated fallback pages using their rendered noindex metadata.
        const file = new URL(`./dist${path}index.html`, import.meta.url);
        return (
          existsSync(file) &&
          !/<meta[^>]+name="robots"[^>]+content="[^"]*\bnoindex\b/.test(
            readFileSync(file, 'utf8'),
          )
        );
      },
      i18n: {
        defaultLocale: 'zh-cn',
        locales: Object.fromEntries(
          Object.entries(languages).map(([locale, definition]) => [
            locale,
            definition.lang,
          ]),
        ),
      },
    }),
    starlight({
      title: 'Ku',
      disable404Route: true,
      description:
        'Ku programming language — documentation and language guide.',
      defaultLocale: 'zh-cn',
      locales: languages,
      plugins: [
        starlightVersions({
          versions: versioning.archives,
          current: { label: versioning.current },
        }),
      ],
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/fengyanweb/ku',
        },
      ],
      customCss: ['./src/styles/tokens.css', './src/styles/docs.css'],
      components: {
        Head: './src/components/DocHead.astro',
        SiteTitle: './src/components/DocSiteTitle.astro',
        PageTitle: './src/components/DocPageTitle.astro',
        LanguageSelect: './src/components/DocLanguageSelect.astro',
        Footer: './src/components/DocFooter.astro',
        SocialIcons: './src/components/DocSocialIcons.astro',
        ThemeSelect: './src/components/DocThemeSelect.astro',
        Search: './src/components/DocSearch.astro',
        Banner: './src/components/DocBanner.astro',
      },
      expressiveCode: {
        shiki: { langs: [kuGrammar] },
      },
      sidebar: [
        {
          label: '开始使用',
          translations: groupTranslations('startGroup'),
          items: [{ slug: 'docs/install' }, { slug: 'docs/getting-started' }],
        },
        {
          label: '语言与工具',
          translations: groupTranslations('toolsGroup'),
          items: [
            { autogenerate: { directory: 'docs/guide' } },
            { autogenerate: { directory: 'docs/tools' } },
          ],
        },
        {
          label: '标准库',
          translations: groupTranslations('libraryGroup'),
          items: [{ autogenerate: { directory: 'docs/stdlib' } }],
        },
        {
          label: '示例',
          translations: groupTranslations('examplesGroup'),
          items: [{ slug: 'docs/examples' }],
        },
        {
          label: '项目',
          translations: groupTranslations('projectGroup'),
          items: [
            { slug: 'docs/support' },
            { slug: 'releases' },
            { slug: 'docs/project' },
          ],
        },
      ],
    }),
  ],
  markdown: { shikiConfig: { langs: [kuGrammar] } },
});
