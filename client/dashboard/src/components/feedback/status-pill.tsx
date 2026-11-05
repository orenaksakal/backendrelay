import type { VariantProps } from 'class-variance-authority';
import { Badge, badgeVariants } from '@/components/ui/badge';
import { cn } from '@/lib/utils';

export type StatusPillStatus =
  | 'active'
  | 'read-only'
  | 'disabled'
  | 'running'
  | 'failed';

interface StatusDefinition {
  label: string;
  tone: NonNullable<VariantProps<typeof badgeVariants>['tone']>;
  /** Only "Running" pulses, and reduced motion stops it (B.4). */
  pulse: boolean;
}

const STATUSES: Record<StatusPillStatus, StatusDefinition> = {
  active: { label: 'Active', tone: 'success', pulse: false },
  'read-only': { label: 'Read-only', tone: 'warning', pulse: false },
  disabled: { label: 'Disabled', tone: 'neutral', pulse: false },
  running: { label: 'Running', tone: 'accent', pulse: true },
  failed: { label: 'Failed', tone: 'destructive', pulse: false },
};

export interface StatusPillProps {
  status: StatusPillStatus;
  /** Overrides the default label while keeping the status colour. */
  label?: string;
  className?: string;
}

/** Neutral pill with a 6px coloured dot and text; colour is never the only signal. */
export function StatusPill({ status, label, className }: StatusPillProps) {
  const definition = STATUSES[status];

  return (
    <Badge tone={definition.tone} className={cn('gap-1.5', className)}>
      <span
        aria-hidden="true"
        className={cn(
          'size-1.5 shrink-0 rounded-full bg-current',
          definition.pulse && 'animate-pulse',
        )}
      />
      {label ?? definition.label}
    </Badge>
  );
}
