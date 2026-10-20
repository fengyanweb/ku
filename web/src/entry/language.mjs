export const PREFERENCE_COOKIE = 'ku_lang';
export const PREFERENCE_MAX_AGE = 31_536_000;

/** @typedef {'zh-cn' | 'en'} EntryLocale */

/**
 * Read one unambiguous, exact cookie value. Cookie values are not URL-decoded
 * or interpolated into a destination. Duplicate names invalidate the preference,
 * including a valid value accompanied by an invalid or identical second value.
 *
 * @param {string | null | undefined} header
 * @returns {EntryLocale | null}
 */
export function readPreferenceCookie(header) {
  if (typeof header !== 'string') return null;
  let occurrences = 0;
  let preference = null;
  for (const part of header.split(';')) {
    // Only the optional whitespace before a cookie-pair is discarded. Keep the
    // value exact: quotes, escapes, extra '=' and whitespace are not aliases.
    const pair = part.replace(/^[ \t]+/, '');
    const separator = pair.indexOf('=');
    const name = separator === -1 ? pair : pair.slice(0, separator);
    if (name !== PREFERENCE_COOKIE) continue;
    occurrences += 1;
    const value = separator === -1 ? null : pair.slice(separator + 1);
    preference = value === 'zh-cn' || value === 'en' ? value : null;
  }
  return occurrences === 1 ? preference : null;
}

/**
 * Serialize only after an explicit user language choice. Other site languages
 * clear the two-language root preference rather than persisting another value.
 * A missing choice returns null; initial renders and automatic entry redirects
 * must not write any cookie. No Domain or HttpOnly attribute is added.
 *
 * @param {unknown} locale
 * @param {boolean} [secure=false] Use the actual page's HTTPS protocol.
 * @returns {string | null}
 */
export function serializePreferenceCookie(locale, secure = false) {
  if (typeof locale !== 'string' || !locale) return null;
  const supported = locale === 'zh-cn' || locale === 'en';
  return (
    `${PREFERENCE_COOKIE}=${supported ? locale : ''}; Path=/; ` +
    `Max-Age=${supported ? PREFERENCE_MAX_AGE : 0}; SameSite=Lax` +
    (secure === true ? '; Secure' : '')
  );
}

/**
 * Input must already come from trusted platform metadata or a local lookup.
 * Missing IP/country metadata, lookup failures, reserved codes and all non-CN
 * countries use English. Browser language, timezone and request headers are
 * deliberately not geolocation sources.
 *
 * @param {unknown} country
 * @returns {EntryLocale}
 */
export function localeForCountry(country) {
  return country === 'CN' ? 'zh-cn' : 'en';
}

/**
 * Handle only the unlocalized root. A null result delegates the unchanged
 * request to the existing static host. The resolver receives no visitor input;
 * an adapter must close over trusted metadata, never a visitor country header.
 * An existing preference bypasses the resolver, including a failing lookup.
 *
 * @param {Request} request
 * @param {{ resolveCountry?: () => unknown | Promise<unknown> }} [options]
 * @returns {Promise<Response | null>}
 */
export async function handleRootEntry(request, options = {}) {
  const url = new URL(request.url);
  if (url.pathname !== '/') return null;

  if (request.method !== 'GET' && request.method !== 'HEAD') {
    return new Response(null, {
      status: 405,
      headers: {
        Allow: 'GET, HEAD',
        'Cache-Control': 'private, no-store',
      },
    });
  }

  let locale = readPreferenceCookie(request.headers.get('Cookie'));
  if (!locale) {
    let country;
    try {
      country = await options.resolveCountry?.();
    } catch {
      // Failure is unknown, not a failed page request or a saved preference.
      country = undefined;
    }
    locale = localeForCountry(country);
  }

  return new Response(null, {
    status: 302,
    headers: {
      // A fixed origin-relative prefix prevents cookie/query open redirects.
      // Fragments are not part of an incoming HTTP request and are not read.
      Location: `/${locale}/${url.search}`,
      'Cache-Control': 'private, no-store',
    },
  });
}
