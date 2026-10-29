import { versions } from './versions';
export const project = {
  name: 'Ku',
  repository: 'https://github.com/fengyanweb/ku',
  release: `https://github.com/fengyanweb/ku/releases/tag/${versions.releaseTag}`,
  version: versions.current,
  releasedVersion: versions.released,
  verifiedAt: '2026-10-28',
  feedbackEmail: 'kagura.gc@qq.com',
} as const;

export { locales, type Locale } from '../i18n/locales';
import type { Locale } from '../i18n/locales';
export const localizedPath = (locale: Locale, topic = '') =>
  `/${locale}/${topic ? `${topic.replace(/^\/+|\/+$/g, '')}/` : ''}`;
