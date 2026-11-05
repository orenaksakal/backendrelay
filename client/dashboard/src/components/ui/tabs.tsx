import { Tabs as TabsPrimitive } from '@base-ui/react/tabs';
import type * as React from 'react';
import { cn } from '@/lib/utils';

/**
 * Underlined tabs, 36px tall. Bind `value`/`onValueChange` to a `?tab=` search
 * param; arrow keys move between tabs (Base UI handles roving focus).
 */
export const Tabs = TabsPrimitive.Root;

type PartProps<T extends React.ElementType> = Omit<
  React.ComponentProps<T>,
  'className'
> & {
  className?: string;
};

export function TabsList({
  className,
  children,
  ...props
}: PartProps<typeof TabsPrimitive.List>) {
  return (
    <TabsPrimitive.List
      data-slot="tabs-list"
      className={cn(
        'border-border relative flex h-9 items-center gap-1 border-b',
        className,
      )}
      {...props}
    >
      {children}
      <TabsPrimitive.Indicator
        data-slot="tabs-indicator"
        className="bg-accent absolute bottom-0 left-0 h-0.5 w-(--active-tab-width) translate-x-(--active-tab-left) transition-[translate,width] duration-[120ms] ease-out"
      />
    </TabsPrimitive.List>
  );
}

export function TabsTrigger({
  className,
  ...props
}: PartProps<typeof TabsPrimitive.Tab>) {
  return (
    <TabsPrimitive.Tab
      data-slot="tabs-trigger"
      className={cn(
        'text-muted-foreground hover:text-foreground data-[active]:text-foreground inline-flex h-9 items-center gap-1.5 px-3 text-sm font-medium whitespace-nowrap transition-colors outline-none select-none data-[disabled]:pointer-events-none data-[disabled]:opacity-50',
        className,
      )}
      {...props}
    />
  );
}

export function TabsContent({
  className,
  ...props
}: PartProps<typeof TabsPrimitive.Panel>) {
  return (
    <TabsPrimitive.Panel
      data-slot="tabs-content"
      className={cn('pt-4 outline-none', className)}
      {...props}
    />
  );
}
