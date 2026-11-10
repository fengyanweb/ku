import { serializePreferenceCookie } from '../entry/language.mjs';

export function rememberLanguageChoice(locale: string) {
  try {
    const cookie = serializePreferenceCookie(
      locale,
      location.protocol === 'https:',
    );
    if (cookie) document.cookie = cookie;
  } catch {
    // Blocking cookies must never block a normal language link.
  }
}

export function updateLanguageLinks() {
  document
    .querySelectorAll<HTMLAnchorElement>('a[data-ku-language]')
    .forEach((link) => {
      link.href = navigationUrl(
        link.href,
        JSON.parse(link.dataset.kuAnchors || '[]'),
        !!link.dataset.kuFallback,
      );
    });
}

export function navigationUrl(
  path: string,
  anchors: string[],
  fallback = false,
) {
  const target = new URL(path, location.origin);
  target.search = location.search;
  target.searchParams.delete('ku_language_fallback');
  if (fallback) target.searchParams.set('ku_language_fallback', '1');
  try {
    target.hash = anchors.includes(decodeURIComponent(location.hash.slice(1)))
      ? location.hash
      : '';
  } catch {
    target.hash = '';
  }
  return target.pathname + target.search + target.hash;
}
