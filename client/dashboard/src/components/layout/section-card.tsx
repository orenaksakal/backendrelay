import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface SectionCardProps {
  title: ReactNode;
  /** 12px helper text under the title. */
  description?: ReactNode;
  /** Header actions, right aligned. */
  actions?: ReactNode;
  children: ReactNode;
  footer?: ReactNode;
  className?: string;
  bodyClassName?: string;
}

/** Card with an `h2`, helper text, header actions and an optional footer (B.4). */
export function SectionCard({
  title,
  description,
  actions,
  children,
  footer,
  className,
  bodyClassName,
}: SectionCardProps) {
  return (
    <section
      className={cn('border-border bg-card rounded-lg border', className)}
    >
      <div className="flex flex-wrap items-start justify-between gap-3 p-4 pb-3">
        <div className="flex min-w-0 flex-col gap-0.5">
          <h2 className="text-section">{title}</h2>
          {description ? (
            <p className="text-muted-foreground text-xs">{description}</p>
          ) : null}
        </div>
        {actions ? (
          <div className="flex shrink-0 items-center gap-2">{actions}</div>
        ) : null}
      </div>
      <div className={cn('p-4 pt-0', bodyClassName)}>{children}</div>
      {footer ? (
        <div className="border-border flex items-center justify-end gap-2 border-t p-4">
          {footer}
        </div>
      ) : null}
    </section>
  );
}
