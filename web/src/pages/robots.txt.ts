import type { APIRoute } from 'astro';
export const GET: APIRoute = ({ site }) => {
  const indexable = import.meta.env.INDEXABLE === 'true' && !!site;
  return new Response(
    indexable
      ? `User-agent: *\nAllow: /\nSitemap: ${new URL('sitemap-index.xml', site)}\n`
      : 'User-agent: *\nDisallow: /\n',
    { headers: { 'Content-Type': 'text/plain; charset=utf-8' } },
  );
};
