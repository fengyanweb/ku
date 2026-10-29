import { getCollection, render } from 'astro:content';
import { locales, localeFromPath, type Locale } from './locales';
import { archiveFromPath, unversionedPath } from '../data/versions';

export interface LanguagePage {
  path: string;
  locale: Locale;
  topic: string;
  anchors: string[];
}

const homeAnchors = [
  'main',
  'hero-title',
  'approach-title',
  'performance-title',
  'practice-title',
  'learn-title',
];
const docs = await getCollection(
  'docs',
  ({ data }) => import.meta.env.DEV || !data.draft,
);
const documentPages = await Promise.all(
  docs.map(async (entry): Promise<LanguagePage | undefined> => {
    const id = entry.id.replace(/(^|\/)index$/, '').replace(/\/$/, '');
    const [locale, ...topic] = id.split('/');
    if (!locales.includes(locale as Locale)) return;
    const { headings } = await render(entry);
    return {
      path: `/${id}/`,
      locale: locale as Locale,
      topic: topic.join('/'),
      anchors: [
        '_top',
        ...headings.map(({ slug }) => slug),
        ...Array.from(
          (entry.body ?? '').matchAll(/\bid=["']([A-Za-z][\w:-]*)["']/g),
          (match) => match[1],
        ),
      ],
    };
  }),
);

// Only actual source entries are included. Starlight's generated untranslated
// fallback routes must not be advertised as real translations.
export const languagePages: LanguagePage[] = [
  ...locales.map((locale) => ({
    path: `/${locale}/`,
    locale,
    topic: '',
    anchors: homeAnchors,
  })),
  ...locales.map((locale) => ({
    path: `/${locale}/support/`,
    locale,
    topic: 'support',
    anchors: ['main'],
  })),
  ...documentPages.filter((page): page is LanguagePage => !!page),
];

const documentPaths = new Set(
  documentPages.filter((page) => !!page).map((page) => page!.path),
);
const documentByPath = new Map(
  documentPages
    .filter((page): page is LanguagePage => !!page)
    .map((page) => [page.path, page]),
);

// Version navigation stays within documentation. A marketing home is not a
// replacement for an absent document, even when that URL exists.
export function versionDestination(path: string, targetSlug?: string): string {
  return versionDestinationInfo(path, targetSlug).path;
}

export function versionDestinationInfo(path: string, targetSlug?: string) {
  const locale = localeFromPath(path);
  const prefix = `/${locale}/${targetSlug ? `${targetSlug}/` : ''}`;
  const topic = unversionedPath(path)
    .split('/')
    .slice(2)
    .filter(Boolean)
    .join('/');
  const samePage = `${prefix}${topic}${topic ? '/' : ''}`;
  if (documentPaths.has(samePage))
    return { ...documentByPath.get(samePage)!, fallback: false };

  const englishSamePage = `/en/${targetSlug ? `${targetSlug}/` : ''}${topic}${topic ? '/' : ''}`;
  if (documentPaths.has(englishSamePage))
    return { ...documentByPath.get(englishSamePage)!, fallback: true };

  const entry = targetSlug ? prefix : `${prefix}docs/getting-started/`;
  if (documentPaths.has(entry))
    return { ...documentByPath.get(entry)!, fallback: false };

  // Older versions do not have every translation. Use a real English entry
  // instead of advertising a generated fallback as a translated document.
  const englishEntry = targetSlug
    ? `/en/${targetSlug}/`
    : '/en/docs/getting-started/';
  if (documentPaths.has(englishEntry))
    return { ...documentByPath.get(englishEntry)!, fallback: true };
  throw new Error(`Missing documentation entry for ${targetSlug ?? 'current'}`);
}

export function translationsForPath(path: string) {
  const current = languagePages.find((page) => page.path === path);
  if (!current) return [];
  return languagePages.filter((page) => page.topic === current.topic);
}

export function languageDestination(path: string, locale: Locale) {
  const sourceLocale = path.split('/')[1] as Locale;
  const topic = path.split('/').slice(2).join('/').replace(/\/$/, '');
  // Recover real target content even when the source URL is a framework fallback.
  // SEO still advertises only actual translated source entries.
  const page = locales.includes(sourceLocale)
    ? languagePages.find(
        (page) => page.locale === locale && page.topic === topic,
      )
    : undefined;
  if (page) return { ...page, fallback: false };

  // A missing translation must preserve the document's version and topic.
  // Marketing pages have their own language-home fallback below.
  if (
    locales.includes(sourceLocale) &&
    (documentPaths.has(path) ||
      topic.startsWith('docs/') ||
      topic === 'releases' ||
      archiveFromPath(path))
  ) {
    const english = documentByPath.get(`/en/${topic}/`);
    if (english) return { ...english, fallback: true };
    const archive = archiveFromPath(path);
    const entry = documentByPath.get(
      archive ? `/en/${archive.slug}/` : '/en/docs/getting-started/',
    );
    if (entry) return { ...entry, fallback: true };
  }
  return {
    path: `/${locale}/`,
    locale,
    anchors: ['ku-language-fallback'],
    fallback: true,
  };
}
