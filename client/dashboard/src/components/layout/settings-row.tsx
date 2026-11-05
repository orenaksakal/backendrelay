import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface SettingsRowProps {
  label: ReactNode;
  description?: ReactNode;
  /** Associates the label with a control by id. */
  htmlFor?: string;
  /** Gives the label text an id for `aria-labelledby` on non-labelable controls. */
  labelId?: string;
  /** The control, on the right; it stacks below `md`. */
  children: ReactNode;
  className?: string;
}

/** One settings line inside a `SectionCard` (B.4). */
export function SettingsRow({
  label,
  description,
  htmlFor,
  labelId,
  children,
  className,
}: SettingsRowProps) {
  return (
    <div
      className={cn(
        'flex flex-col gap-2 md:flex-row md:items-center md:justify-between md:gap-6',
        className,
      )}
    >
      <div className="flex max-w-[360px] flex-col gap-0.5">
        {htmlFor ? (
          <label id={labelId} htmlFor={htmlFor} className="text-sm font-medium">
            {label}
          </label>
        ) : (
          <span id={labelId} className="text-sm font-medium">
            {label}
          </span>
        )}
        {description ? (
          <p className="text-muted-foreground text-xs">{description}</p>
        ) : null}
      </div>
      <div className="flex shrink-0 items-center gap-2">{children}</div>
    </div>
  );
}
