import { Input as InputPrimitive } from '@base-ui/react/input';
import { type ComponentProps } from 'react';
import { cn } from '@instantdb/components';

function Input({ className, ...props }: ComponentProps<'input'>) {
  return (
    <InputPrimitive
      data-slot="input"
      className={cn('dash-input', className)}
      {...props}
    />
  );
}

export { Input };
