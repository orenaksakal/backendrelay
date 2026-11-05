import { Button as ButtonPrimitive } from '@base-ui/react/button';
import { cva, type VariantProps } from 'class-variance-authority';
import { cn } from '@instantdb/components';
import { forwardRef } from 'react';

const buttonVariants = cva('dash-button', {
  variants: {
    variant: {
      default: 'dash-button-default',
      outline: 'dash-button-outline',
      secondary: 'dash-button-secondary',
      ghost: 'dash-button-ghost',
      destructive: 'dash-button-destructive',
      link: 'dash-button-link',
    },
    size: {
      default: 'dash-button-default-size',
      xs: 'dash-button-xs',
      sm: 'dash-button-sm',
      lg: 'dash-button-lg',
      icon: 'dash-button-icon',
      'icon-xs': 'dash-button-icon-xs',
      'icon-sm': 'dash-button-icon-sm',
      'icon-lg': 'dash-button-icon-lg',
    },
  },
  defaultVariants: {
    variant: 'default',
    size: 'default',
  },
});

const Button = forwardRef<
  HTMLButtonElement,
  ButtonPrimitive.Props & VariantProps<typeof buttonVariants>
>(function Button({ className, variant, size, ...props }, ref) {
  return (
    <ButtonPrimitive
      ref={ref}
      data-slot="button"
      className={cn(buttonVariants({ variant, size }), className)}
      {...props}
    />
  );
});

export { Button, buttonVariants };
