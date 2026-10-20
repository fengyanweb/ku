if (!process.env.SITE_URL || process.env.INDEXABLE !== 'true') {
  throw new Error(
    'Production build needs the confirmed SITE_URL=https://… and INDEXABLE=true. Use npm run build for a noindex local preview.',
  );
}
