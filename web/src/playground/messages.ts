import english from './i18n/en.json';
import type { Locale } from '../i18n/locales';
import languages from '../i18n/languages.json';

export type PlaygroundMessages = typeof english;
const dictionaries = import.meta.glob<PlaygroundMessages>('./i18n/*.json', {
  eager: true,
  import: 'default',
});

// This module runs during static generation; the browser receives only its
// page's dictionary. Fail the build rather than publish a partial translation.
for (const locale of Object.keys(languages)) {
  const messages = dictionaries[`./i18n/${locale}.json`];
  if (!messages) throw new Error(`Missing Playground translation: ${locale}`);
  for (const section of ['page', 'status', 'examples'] as const) {
    const translated = messages[section];
    if (
      !translated ||
      Object.keys(english[section]).sort().join(',') !==
        Object.keys(translated).sort().join(',') ||
      Object.values(translated).some(
        (value) => typeof value !== 'string' || !value.trim(),
      )
    )
      throw new Error(
        `Incomplete Playground translation: ${locale}.${section}`,
      );
  }
}

export function playgroundMessages(locale: Locale): PlaygroundMessages {
  const messages = dictionaries[`./i18n/${locale}.json`];
  if (!messages) throw new Error(`Missing Playground translation: ${locale}`);
  return messages;
}
