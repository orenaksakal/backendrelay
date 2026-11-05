import { Field as FieldPrimitive } from '@base-ui/react/field';
import type * as React from 'react';
import { Separator } from '@/components/ui/separator';
import { cn } from '@/lib/utils';

export type FieldOrientation = 'vertical' | 'horizontal' | 'responsive';

export type FieldProps = Omit<
  React.ComponentProps<typeof FieldPrimitive.Root>,
  'className'
> & {
  className?: string;
  /** `horizontal` and `responsive` place the control beside the label. */
  orientation?: FieldOrientation;
};

/**
 * A form field. Base UI's field context wires the label, description and error
 * to the control with `htmlFor`, `aria-describedby` and `aria-invalid`.
 */
export function Field({
  className,
  orientation = 'vertical',
  ...props
}: FieldProps) {
  return (
    <FieldPrimitive.Root
      data-slot="field"
      className={cn(
        'flex w-full gap-1.5',
        orientation === 'vertical' && 'flex-col',
        orientation === 'horizontal' && 'flex-row items-center justify-between',
        orientation === 'responsive' &&
          'flex-col md:flex-row md:items-center md:justify-between md:gap-6',
        'data-[disabled]:opacity-50',
        className,
      )}
      {...props}
    />
  );
}

export type FieldLabelProps = Omit<
  React.ComponentProps<typeof FieldPrimitive.Label>,
  'className'
> & { className?: string };

export function FieldLabel({ className, ...props }: FieldLabelProps) {
  return (
    <FieldPrimitive.Label
      data-slot="field-label"
      className={cn('text-sm font-medium', className)}
      {...props}
    />
  );
}

export type FieldDescriptionProps = Omit<
  React.ComponentProps<typeof FieldPrimitive.Description>,
  'className'
> & { className?: string };

export function FieldDescription({
  className,
  ...props
}: FieldDescriptionProps) {
  return (
    <FieldPrimitive.Description
      data-slot="field-description"
      className={cn('text-muted-foreground text-xs', className)}
      {...props}
    />
  );
}

export type FieldErrorProps = Omit<
  React.ComponentProps<typeof FieldPrimitive.Error>,
  'className'
> & { className?: string };

/** Shown only while the field is invalid; associated through the field context. */
export function FieldError({ className, ...props }: FieldErrorProps) {
  return (
    <FieldPrimitive.Error
      data-slot="field-error"
      className={cn('text-destructive text-xs font-medium', className)}
      {...props}
    />
  );
}

export type FieldControlProps = Omit<
  React.ComponentProps<typeof FieldPrimitive.Control>,
  'className'
> & { className?: string };

/** A native `<input>` that participates in the field's validation state. */
export function FieldControl({ className, ...props }: FieldControlProps) {
  return (
    <FieldPrimitive.Control
      data-slot="field-control"
      className={cn(
        'border-input bg-card placeholder:text-muted-foreground focus-visible:border-ring aria-invalid:border-destructive h-8 w-full min-w-0 rounded-md border px-2.5 py-1 text-sm transition-colors disabled:cursor-not-allowed disabled:opacity-50',
        className,
      )}
      {...props}
    />
  );
}

export type FieldGroupProps = Omit<React.ComponentProps<'div'>, 'className'> & {
  className?: string;
};

export function FieldGroup({ className, ...props }: FieldGroupProps) {
  return (
    <div
      data-slot="field-group"
      className={cn('flex w-full flex-col gap-4', className)}
      {...props}
    />
  );
}

export type FieldSetProps = Omit<
  React.ComponentProps<'fieldset'>,
  'className'
> & { className?: string };

export function FieldSet({ className, ...props }: FieldSetProps) {
  return (
    <fieldset
      data-slot="field-set"
      className={cn('flex flex-col gap-4', className)}
      {...props}
    />
  );
}

export type FieldLegendProps = Omit<
  React.ComponentProps<'legend'>,
  'className'
> & { className?: string };

export function FieldLegend({ className, ...props }: FieldLegendProps) {
  return (
    <legend
      data-slot="field-legend"
      className={cn('text-section', className)}
      {...props}
    />
  );
}

export type FieldContentProps = Omit<
  React.ComponentProps<'div'>,
  'className'
> & { className?: string };

export function FieldContent({ className, ...props }: FieldContentProps) {
  return (
    <div
      data-slot="field-content"
      className={cn('flex min-w-0 flex-col gap-1', className)}
      {...props}
    />
  );
}

export type FieldSeparatorProps = Omit<
  React.ComponentProps<'div'>,
  'className'
> & { className?: string };

export function FieldSeparator({
  children,
  className,
  ...props
}: FieldSeparatorProps) {
  return (
    <div
      data-slot="field-separator"
      className={cn(
        'text-muted-foreground relative flex h-4 items-center text-xs',
        className,
      )}
      {...props}
    >
      <Separator className="absolute inset-x-0" />
      <span className="bg-background relative mx-auto px-2">{children}</span>
    </div>
  );
}
