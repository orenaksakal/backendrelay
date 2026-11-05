import {
  Children,
  Fragment,
  forwardRef,
  useCallback,
  useState,
  isValidElement,
  type ReactNode,
  type ComponentProps,
} from 'react';
import { Select } from '@base-ui/react/select';
import { Check, ChevronDown } from 'lucide-react';
import './dashboard-select.css';

type Option = { value: string; label: ReactNode; disabled?: boolean };
function optionsFrom(children: ReactNode): Option[] {
  return Children.toArray(children).flatMap((child): Option[] => {
    if (
      !isValidElement<{
        value?: string | number;
        children?: ReactNode;
        disabled?: boolean;
      }>(child)
    )
      return [];
    if (child.type === Fragment || child.type === 'optgroup')
      return optionsFrom(child.props.children);
    return [
      {
        value: String(child.props.value ?? child.props.children ?? ''),
        label: child.props.children,
        disabled: child.props.disabled,
      },
    ];
  });
}

type Props = Omit<
  ComponentProps<'button'>,
  'onChange' | 'value' | 'defaultValue'
> & {
  value?: string | number;
  defaultValue?: string | number;
  required?: boolean;
  onChange?: (event: {
    target: { value: string };
    currentTarget: { value: string };
  }) => void;
};
/** Shared dashboard select: retains option markup while using Base UI for keyboard and popup behavior. */
export const DashboardSelect = forwardRef<HTMLButtonElement, Props>(
  function DashboardSelect(
    {
      children,
      value,
      defaultValue,
      onChange,
      disabled,
      name,
      required,
      id,
      className = '',
      ...props
    },
    ref,
  ) {
    const options = optionsFrom(children);
    const [container, setContainer] = useState<HTMLElement | null>(null);
    const triggerRef = useCallback(
      (node: HTMLButtonElement | null) => {
        if (typeof ref === 'function') ref(node);
        else if (ref) ref.current = node;
        if (node) setContainer(node.closest<HTMLElement>('[role="dialog"]'));
      },
      [ref],
    );
    return (
      <Select.Root
        items={options}
        value={value === undefined ? undefined : String(value)}
        defaultValue={
          defaultValue === undefined ? options[0]?.value : String(defaultValue)
        }
        onValueChange={(next) => {
          if (next !== null)
            onChange?.({
              target: { value: next },
              currentTarget: { value: next },
            });
        }}
        disabled={disabled}
        name={name}
        required={required}
      >
        <Select.Trigger
          ref={triggerRef}
          {...props}
          id={id}
          className={`dashboard-select ${className}`}
          data-slot="select-trigger"
        >
          <Select.Value />
          <Select.Icon className="dashboard-select-icon">
            <ChevronDown size={14} />
          </Select.Icon>
        </Select.Trigger>
        <Select.Portal container={container ?? undefined}>
          <Select.Positioner
            sideOffset={6}
            align="start"
            alignItemWithTrigger={false}
            className="dashboard-select-positioner"
          >
            <Select.Popup className="dashboard-select-popup">
              <Select.List>
                {options.map((option) => (
                  <Select.Item
                    key={option.value}
                    value={option.value}
                    disabled={option.disabled}
                    className="dashboard-select-option"
                  >
                    <Select.ItemText>{option.label}</Select.ItemText>
                    <Select.ItemIndicator>
                      <Check size={14} />
                    </Select.ItemIndicator>
                  </Select.Item>
                ))}
              </Select.List>
            </Select.Popup>
          </Select.Positioner>
        </Select.Portal>
      </Select.Root>
    );
  },
);
