/**
 * Returns the last path segment of a filesystem path, normalising both
 * forward-slash and backslash separators and stripping any trailing slashes.
 *
 * Examples:
 *   folderNameFromPath('/home/user/.agents/skills/my-skill') → 'my-skill'
 *   folderNameFromPath('C:\\agents\\skills\\my-skill\\')   → 'my-skill'
 */
export function folderNameFromPath(path: string): string {
  const normalized = path.replace(/\\/g, '/').replace(/\/+$/, '');
  const segments = normalized.split('/');
  return segments[segments.length - 1] || normalized;
}
