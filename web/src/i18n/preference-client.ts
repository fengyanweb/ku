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
      const target = new URL(link.href);
      target.search = location.search;
      if (!link.dataset.kuFallback) {
        const anchors: string[] = JSON.parse(link.dataset.kuAnchors || '[]');
        try {
          target.hash = anchors.includes(
            decodeURIComponent(location.hash.slice(1)),
          )
            ? location.hash
            : '';
        } catch {
          target.hash = '';
        }
      }
      link.href = target.pathname + target.search + target.hash;
    });
}
