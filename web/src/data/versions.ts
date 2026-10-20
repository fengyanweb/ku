import versions from './versions.json';
export { versions };
export function archiveFromPath(path: string) {
  return versions.archives.find(
    (version) => path.split('/')[2] === version.slug,
  );
}
export function unversionedPath(path: string) {
  const archive = archiveFromPath(path);
  return archive ? path.replace(`/${archive.slug}/`, '/') : path;
}
