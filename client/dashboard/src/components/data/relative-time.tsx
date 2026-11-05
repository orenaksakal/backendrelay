import { formatIsoTime, formatRelativeTime } from '@/lib/format';
import { cn } from '@/lib/utils';

export interface RelativeTimeProps {
  /** ISO string, epoch milliseconds or a Date. */
  value: string | number | Date;
  /** Fixed "now" for tests and screenshots. */
  now?: Date;
  className?: string;
}

/**
 * `3 min ago` in a `<time>` element, with the exact ISO timestamp in the
 * native tooltip and in screen-reader-only text (B.4).
 */
export function RelativeTime({ value, now, className }: RelativeTimeProps) {
  const date = value instanceof Date ? value : new Date(value);

  if (Number.isNaN(date.getTime())) {
    return <span className={cn('text-muted-foreground', className)}>—</span>;
  }

  const iso = formatIsoTime(date);

  return (
    <time dateTime={iso} title={iso} className={cn('tabular-nums', className)}>
      {formatRelativeTime(date, now)}
      <span className="sr-only"> ({iso})</span>
    </time>
  );
}
