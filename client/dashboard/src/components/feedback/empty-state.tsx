import type { LucideIcon } from 'lucide-react';
import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface EmptyStateProps {
  icon: LucideIcon;
  title: string;
  description?: ReactNode;
  /** Primary action, for example "Create app". */
  action?: ReactNode;
  /** Optional secondary link or button. */
  secondary?: ReactNode;
  className?: string;
}

/** Centred empty state, 420px wide, with the next action (B.4, B.5.10). */
export function EmptyState({
  icon: Icon,
  title,
  description,
  action,
  secondary,
  className,
}: EmptyStateProps) {
  return (
    <div
      className={cn(
        'mx-auto flex max-w-[420px] flex-col items-center gap-2 px-4 py-10 text-center',
        className,
      )}
    >
      <Icon className="text-muted-foreground size-6" aria-hidden="true" />
      <p className="text-section">{title}</p>
      {description ? (
        <p className="text-muted-foreground text-sm">{description}</p>
      ) : null}
      {action ? <div className="mt-2">{action}</div> : null}
      {secondary ? <div className="mt-1">{secondary}</div> : null}
    </div>
  );
}
