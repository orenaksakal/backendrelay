/** Shared value formatters (B.4 `Bytes`, `Count`, `RelativeTime`). */

const BYTE_UNITS = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'] as const;

/**
 * Byte size with one decimal place: `1.2 MB`, `220 KB`. Base 1024, matching
 * the storage figures the server reports.
 */
export function formatBytes(value: number): string {
  if (!Number.isFinite(value)) {
    return '—';
  }
  const sign = value < 0 ? '-' : '';
  let remaining = Math.abs(value);
  let unit = 0;
  while (remaining >= 1024 && unit < BYTE_UNITS.length - 1) {
    remaining /= 1024;
    unit += 1;
  }
  const rounded =
    unit === 0 ? Math.round(remaining) : Math.round(remaining * 10) / 10;
  return `${sign}${String(rounded)} ${BYTE_UNITS[unit] ?? 'B'}`;
}

/** Grouped integer: `12,345`. */
export function formatCount(value: number): string {
  return new Intl.NumberFormat('en-US').format(value);
}

/** Short identifier form: `3f2a…9c1d`. */
export function truncateId(value: string, head = 4, tail = 4): string {
  if (value.length <= head + tail + 1) {
    return value;
  }
  return `${value.slice(0, head)}…${value.slice(-tail)}`;
}

const MINUTE = 60;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const WEEK = 7 * DAY;
const MONTH = 30 * DAY;
const YEAR = 365 * DAY;

function plural(count: number, unit: string): string {
  return `${formatCount(count)} ${unit}`;
}

function toDate(value: string | number | Date): Date {
  return value instanceof Date ? value : new Date(value);
}

/** Exact, machine-readable time for `title` attributes and `<time>`. */
export function formatIsoTime(value: string | number | Date): string {
  return toDate(value).toISOString();
}

/**
 * Compact relative time: `just now`, `3 min ago`, `in 2 d`. Past and future
 * values both format, and anything unparsable falls back to an em dash.
 */
export function formatRelativeTime(
  value: string | number | Date,
  now: Date = new Date(),
): string {
  const date = toDate(value);
  if (Number.isNaN(date.getTime())) {
    return '—';
  }

  const seconds = (date.getTime() - now.getTime()) / 1000;
  const magnitude = Math.abs(seconds);
  const future = seconds > 0;
  const wrap = (text: string) => (future ? `in ${text}` : `${text} ago`);

  if (magnitude < 45) {
    return 'just now';
  }
  if (magnitude < HOUR) {
    return wrap(plural(Math.round(magnitude / MINUTE), 'min'));
  }
  if (magnitude < DAY) {
    return wrap(plural(Math.round(magnitude / HOUR), 'h'));
  }
  if (magnitude < WEEK) {
    return wrap(plural(Math.round(magnitude / DAY), 'd'));
  }
  if (magnitude < MONTH) {
    return wrap(plural(Math.round(magnitude / WEEK), 'w'));
  }
  if (magnitude < YEAR) {
    return wrap(plural(Math.round(magnitude / MONTH), 'mo'));
  }
  return wrap(plural(Math.round(magnitude / YEAR), 'y'));
}
