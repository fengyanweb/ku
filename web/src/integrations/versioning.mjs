import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import starlightVersions from 'starlight-versions';

// The upstream plugin detects existing archives using root directories. A fully
// localized snapshot otherwise leaves this root empty, which Git cannot retain.
function retainArchiveMarkers(archives, srcDir) {
  for (const archive of archives) {
    if (!existsSync(new URL(`content/versions/${archive.slug}.json`, srcDir)))
      continue;
    const root = new URL(`content/docs/${archive.slug}/`, srcDir);
    mkdirSync(root, { recursive: true });
    const marker = new URL('.gitkeep', root);
    if (!existsSync(marker))
      writeFileSync(
        marker,
        'Preserve the version directory used to identify this documentation archive.\n',
        { flag: 'wx' },
      );
  }
}

export default function versioning(options) {
  const plugin = starlightVersions(options);
  const setup = plugin.hooks['config:setup'];
  return {
    ...plugin,
    hooks: {
      ...plugin.hooks,
      async 'config:setup'(context) {
        // Recover the detection marker before upstream considers creating a new
        // snapshot, including after checkout on another machine.
        retainArchiveMarkers(options.versions, context.astroConfig.srcDir);
        await setup(context);
        retainArchiveMarkers(options.versions, context.astroConfig.srcDir);
      },
    },
  };
}
