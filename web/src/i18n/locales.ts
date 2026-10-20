import english from './en.json';
import languages from './languages.json';
import { versions } from '../data/versions';

export type Locale = keyof typeof languages;
export const localeDefinitions = languages as Record<
  Locale,
  { label: string; lang: string; dir: 'ltr' | 'rtl' }
>;
export const locales = Object.keys(localeDefinitions) as Locale[];
export type Translation = typeof english;
const dictionaries = import.meta.glob<Translation>('./*.json', {
  eager: true,
  import: 'default',
});
export function translate(locale: Locale): Translation {
  const dictionary = dictionaries[`./${locale}.json`];
  if (!dictionary) throw new Error(`Missing locale dictionary: ${locale}`);
  const interpolate = (value: string) =>
    value
      .replaceAll('{currentVersion}', versions.current)
      .replaceAll('{releasedVersion}', versions.released);
  return {
    ...dictionary,
    nextNotice: interpolate(dictionary.nextNotice),
    projectDescription: interpolate(dictionary.projectDescription),
  };
}
export function localeFromPath(path: string): Locale {
  const segment = path.split('/')[1] as Locale;
  return segment in localeDefinitions ? segment : 'zh-cn';
}
export function languagePath(path: string, locale: Locale): string {
  const segments = path.split('/');
  segments[1] = locale;
  return segments.join('/');
}
