import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface PageHeaderProps {
  title: string;
  description?: ReactNode;
  /** Right-aligned actions: at most one primary plus secondaries (B.4). */
  actions?: ReactNode;
  /** Optional tabs below the title. */
  tabs?: ReactNode;
  className?: string;
}

/**
 * Page title block. The `h1` carries `id="page-title"` and `tabIndex={-1}` so
 * navigation can move focus to it (B.5.8).
 */
export function PageHeader({
  title,
  description,
  actions,
  tabs,
  className,
}: PageHeaderProps) {
  return (
    <header className={cn('flex flex-col gap-3', className)}>
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="flex min-w-0 flex-col gap-1">
          <h1 id="page-title" tabIndex={-1} className="text-page">
            {title}
          </h1>
          {description ? (
            <div className="text-muted-foreground text-sm">{description}</div>
          ) : null}
        </div>
        {actions ? (
          <div className="flex shrink-0 items-center gap-2">{actions}</div>
        ) : null}
      </div>
      {tabs}
    </header>
  );
}
