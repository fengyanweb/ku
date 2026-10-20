import { defineCollection } from 'astro:content';
import { z } from 'astro/zod';
import { docsVersionsLoader } from 'starlight-versions/loader';
import { docsLoader, i18nLoader } from '@astrojs/starlight/loaders';
import { docsSchema, i18nSchema } from '@astrojs/starlight/schema';

export const collections = {
  versions: defineCollection({ loader: docsVersionsLoader() }),
  i18n: defineCollection({ loader: i18nLoader(), schema: i18nSchema() }),
  docs: defineCollection({
    loader: docsLoader(),
    schema: docsSchema({
      extend: z.object({
        kuVersion: z.string(),
        verifiedAt: z.preprocess(
          (value) =>
            value instanceof Date ? value.toISOString().slice(0, 10) : value,
          z.string().regex(/^\d{4}-\d{2}-\d{2}$/),
        ),
        sources: z.array(z.string()).min(1),
      }),
    }),
  }),
};
