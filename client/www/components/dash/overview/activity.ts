export type Activity = { count: number; origins: Record<string, number> };

export function parseActivity(input: unknown): Activity {
  if (
    !input ||
    typeof input !== 'object' ||
    !('count' in input) ||
    !('origins' in input)
  )
    throw new Error('The server returned an invalid activity snapshot.');
  const { count, origins } = input;
  if (
    typeof count !== 'number' ||
    !Number.isSafeInteger(count) ||
    count < 0 ||
    !origins ||
    typeof origins !== 'object' ||
    Array.isArray(origins)
  )
    throw new Error('The server returned an invalid activity snapshot.');
  const pairs = Object.entries(origins);
  if (
    pairs.some(
      ([, n]) => typeof n !== 'number' || !Number.isSafeInteger(n) || n < 0,
    )
  )
    throw new Error('The server returned invalid origin counts.');
  return { count, origins: Object.fromEntries(pairs) };
}
