import { versions } from './versions';
export const project = {
  name: 'Ku',
  repository: 'https://github.com/fengyanweb/ku',
  release: versions.releaseUrl,
  version: versions.languageVersion,
  releasedVersion: versions.released,
  releaseTag: versions.releaseTag,
  releaseCommit: versions.releaseCommit,
  docsSourceRef: versions.docsSourceRef,
  verifiedAt: versions.verifiedAt,
  feedbackEmail: 'kagura.gc@qq.com',
} as const;

export { locales, type Locale } from '../i18n/locales';
import type { Locale } from '../i18n/locales';
export const localizedPath = (locale: Locale, topic = '') =>
  `/${locale}/${topic ? `${topic.replace(/^\/+|\/+$/g, '')}/` : ''}`;
