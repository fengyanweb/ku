import { readFileSync, writeFileSync } from 'node:fs';

// starlight-versions 0.10.1 recursively copies older localized archives into
// newly created versions. Keep this one-condition patch explicit and bounded
// until an upstream release fixes the verified regression.
const packageFile = new URL(
  '../node_modules/starlight-versions/package.json',
  import.meta.url,
);
const sourceFile = new URL(
  '../node_modules/starlight-versions/libs/versions.ts',
  import.meta.url,
);
const { version } = JSON.parse(readFileSync(packageFile, 'utf8'));
if (version !== '0.10.1') {
  throw new Error(
    `Review the localized archive patch before using starlight-versions ${version}.`,
  );
}
const original =
  'secondLastSegment && lastSegment === newVersion.slug && locales.includes(secondLastSegment)';
const patched =
  'secondLastSegment && lastSegment && lastSegment in config.versionsBySlug && locales.includes(secondLastSegment)';
const source = readFileSync(sourceFile, 'utf8');
const count = (text) => source.split(text).length - 1;
if (count(patched) === 1 && count(original) === 0) {
  console.log('starlight-versions localized archive patch already applied.');
} else if (count(original) === 1 && count(patched) === 0) {
  writeFileSync(sourceFile, source.replace(original, patched));
  console.log(
    'Patched starlight-versions 0.10.1 to preserve existing localized archives.',
  );
} else {
  throw new Error(
    'Unexpected starlight-versions source; review the localized archive patch.',
  );
}
