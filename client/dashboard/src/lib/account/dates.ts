/**
 * The server returns `NaiveDateTime` values without a timezone. The old
 * dashboard treated those as UTC; keep that behaviour so the relative times
 * stay correct across browsers (C.18).
 */
export function accountTimestamp(value: string | number): string | number {
  if (
    typeof value === 'string' &&
    /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?$/.test(value)
  ) {
    return `${value}Z`;
  }
  return value;
}
