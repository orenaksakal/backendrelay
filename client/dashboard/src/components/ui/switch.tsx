import { Switch as SwitchPrimitive } from '@base-ui/react/switch';
import type * as React from 'react';
import { cn } from '@/lib/utils';

export type SwitchProps = Omit<
  React.ComponentProps<typeof SwitchPrimitive.Root>,
  'className'
> & { className?: string };

/** On/off control for settings that apply immediately. */
export function Switch({ className, ...props }: SwitchProps) {
  return (
    <SwitchPrimitive.Root
      data-slot="switch"
      className={cn(
        'border-input bg-muted data-[checked]:border-primary data-[checked]:bg-primary inline-flex h-6 w-10 shrink-0 items-center rounded-full border transition-colors disabled:cursor-not-allowed disabled:opacity-50',
        className,
      )}
      {...props}
    >
      <SwitchPrimitive.Thumb className="bg-card block size-4 translate-x-[3px] rounded-full transition-transform data-[checked]:translate-x-[19px]" />
    </SwitchPrimitive.Root>
  );
}
