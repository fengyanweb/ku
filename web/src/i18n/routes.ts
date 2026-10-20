import { getCollection, render } from 'astro:content';
import { locales, type Locale } from './locales';

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
      anchors: ['_top', ...headings.map(({ slug }) => slug)],
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
  ...documentPages.filter((page): page is LanguagePage => !!page),
];

export function translationsForPath(path: string) {
  const current = languagePages.find((page) => page.path === path);
  if (!current) return [];
  return languagePages.filter((page) => page.topic === current.topic);
}

export function languageDestination(path: string, locale: Locale) {
  const page = translationsForPath(path).find((page) => page.locale === locale);
  return page
    ? { ...page, fallback: false }
    : {
        path: `/${locale}/#ku-language-fallback`,
        locale,
        anchors: ['ku-language-fallback'],
        fallback: true,
      };
}
