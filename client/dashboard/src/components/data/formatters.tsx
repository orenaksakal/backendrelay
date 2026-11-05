import { formatBytes, formatCount } from '@/lib/format';
import { cn } from '@/lib/utils';

export interface BytesProps {
  value: number;
  className?: string;
}

/** `1.2 MB`, right-aligned and tabular wherever it lands in a column. */
export function Bytes({ value, className }: BytesProps) {
  return (
    <span
      className={cn('tabular-nums', className)}
      title={`${String(value)} bytes`}
    >
      {formatBytes(value)}
    </span>
  );
}

export interface CountProps {
  value: number;
  className?: string;
}

/** `12,345` with tabular numbers. */
export function Count({ value, className }: CountProps) {
  return (
    <span className={cn('tabular-nums', className)}>{formatCount(value)}</span>
  );
}
