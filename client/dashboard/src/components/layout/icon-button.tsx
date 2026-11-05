import type * as React from 'react';
import { Button, type ButtonProps } from '@/components/ui/button';
import { Kbd } from '@/components/ui/kbd';
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from '@/components/ui/tooltip';
import { cn } from '@/lib/utils';

export interface IconButtonProps
  extends Omit<ButtonProps, 'aria-label' | 'children' | 'className' | 'size'> {
  /** Required: the icon carries no text (B.4). */
  'aria-label': string;
  icon: React.ReactNode;
  /** Shown in the tooltip beside the label, for example `⌘C`. */
  shortcut?: string;
  size?: 'sm' | 'md';
  className?: string;
}

/**
 * Square icon button, 28 or 32px, with a tooltip that repeats the accessible
 * name and its shortcut. `children` is deliberately replaced by `icon` so the
 * label cannot be dropped.
 */
export function IconButton({
  'aria-label': label,
  icon,
  shortcut,
  size = 'md',
  variant = 'ghost',
  className,
  ...props
}: IconButtonProps) {
  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <Button
            type="button"
            variant={variant}
            size={size}
            aria-label={label}
            className={cn(
              size === 'sm' ? 'size-7 p-0' : 'size-8 p-0',
              className,
            )}
            {...props}
          />
        }
      >
        {icon}
      </TooltipTrigger>
      <TooltipContent>
        {label}
        {shortcut ? <Kbd className="ml-1.5">{shortcut}</Kbd> : null}
      </TooltipContent>
    </Tooltip>
  );
}
