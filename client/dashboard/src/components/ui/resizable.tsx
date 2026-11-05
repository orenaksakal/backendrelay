import { Group, Panel, Separator } from 'react-resizable-panels';
import type * as React from 'react';
import { cn } from '@/lib/utils';

/**
 * Resizable panes on react-resizable-panels v4 (`Group`/`Panel`/`Separator`;
 * not `PanelGroup`/`PanelResizeHandle`). The separator is a keyboard-operable
 * splitter, so resizing has a non-drag alternative (WCAG 2.5.7).
 */
export const ResizablePanel = Panel;

export function ResizablePanelGroup({
  className,
  ...props
}: React.ComponentProps<typeof Group> & { className?: string }) {
  return (
    <Group
      data-slot="resizable-panel-group"
      className={cn(
        'flex h-full w-full data-[orientation=vertical]:flex-col',
        className,
      )}
      {...props}
    />
  );
}

export function ResizableHandle({
  className,
  ...props
}: React.ComponentProps<typeof Separator> & { className?: string }) {
  return (
    <Separator
      data-slot="resizable-handle"
      className={cn(
        'bg-border hover:bg-input focus-visible:outline-ring relative flex w-px shrink-0 items-center justify-center transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 data-[orientation=vertical]:h-px data-[orientation=vertical]:w-full',
        className,
      )}
      {...props}
    />
  );
}
