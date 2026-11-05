import { cn } from '@/lib/utils';

export interface BackupProgressProps {
  /** 0–100, or null while the total is unknown. */
  value: number | null;
  label: string;
  className?: string;
}

/** Small determinate bar used by backup/restore jobs and downloads (C.15). */
export function BackupProgress({
  value,
  label,
  className,
}: BackupProgressProps) {
  const percent = value === null ? null : Math.max(0, Math.min(100, value));
  return (
    <div className={cn('flex min-w-32 items-center gap-2', className)}>
      <div
        role="progressbar"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={percent ?? undefined}
        aria-valuetext={percent === null ? 'Estimating' : `${String(percent)}%`}
        className="bg-muted h-1.5 min-w-16 flex-1 overflow-hidden rounded-full"
      >
        <div
          className="bg-accent h-full rounded-full transition-[width] duration-150"
          style={{ width: `${String(percent ?? 0)}%` }}
        />
      </div>
      <span className="text-muted-foreground w-10 text-right text-xs tabular-nums">
        {percent === null ? '—' : `${String(percent)}%`}
      </span>
    </div>
  );
}
