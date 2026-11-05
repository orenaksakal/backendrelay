import { defaultParseSearch } from '@tanstack/react-router';

// nuqs owns JSON decoding for these keys. Its TanStack adapter treats native
// arrays as repeated parameters, which destroys nested filter/where tuples.
export function parseDashboardSearch(search: string) {
  const parsed: Record<string, unknown> = defaultParseSearch(search);
  const raw = new URLSearchParams(search);
  for (const key of ['filters', 'where']) {
    const value = raw.get(key);
    if (value !== null) parsed[key] = value;
  }
  return parsed;
}
