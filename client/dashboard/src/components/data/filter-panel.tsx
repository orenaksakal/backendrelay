import { Filter, Plus, Trash2 } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Input } from '@/components/ui/input';
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from '@/components/ui/popover';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { IconButton } from '@/components/layout/icon-button';
import { cn } from '@/lib/utils';
import {
  FILTER_OPERATORS,
  createFilterCondition,
  isOperatorSupported,
  type FilterAttribute,
  type FilterCondition,
  type FilterOperator,
} from './filter-serialization';

export interface FilterPanelProps {
  conditions: ReadonlyArray<FilterCondition>;
  onChange: (conditions: FilterCondition[]) => void;
  attributes: ReadonlyArray<FilterAttribute>;
  className?: string;
}

function replaceAt(
  conditions: ReadonlyArray<FilterCondition>,
  id: string,
  patch: Partial<FilterCondition>,
): FilterCondition[] {
  return conditions.map((condition) =>
    condition.id === id ? { ...condition, ...patch } : condition,
  );
}

/** Editable condition rows plus Add filter / Clear filters (B.4). */
export function FilterPanel({
  conditions,
  onChange,
  attributes,
  className,
}: FilterPanelProps) {
  const attributeTypes = new Map(
    attributes.map((attribute) => [attribute.name, attribute]),
  );

  return (
    <div className={cn('flex flex-col gap-3', className)}>
      <div className="flex flex-col gap-2">
        {conditions.map((condition, index) => (
          <div
            key={condition.id}
            className="flex flex-wrap items-center gap-2 sm:flex-nowrap"
          >
            <Checkbox
              aria-label={`Enable filter ${String(index + 1)}`}
              checked={condition.enabled}
              onCheckedChange={(checked) => {
                onChange(
                  replaceAt(conditions, condition.id, {
                    enabled: checked,
                  }),
                );
              }}
            />
            <Select
              value={condition.attribute}
              onValueChange={(value) => {
                onChange(
                  replaceAt(conditions, condition.id, {
                    attribute: typeof value === 'string' ? value : '',
                  }),
                );
              }}
            >
              <SelectTrigger
                aria-label={`Attribute for filter ${String(index + 1)}`}
                className="w-40"
              >
                <SelectValue placeholder="Attribute" />
              </SelectTrigger>
              <SelectContent>
                {attributes.map((attribute) => (
                  <SelectItem key={attribute.name} value={attribute.name}>
                    {attribute.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Select
              value={condition.operator}
              onValueChange={(value) => {
                const operator: FilterOperator =
                  typeof value === 'string' &&
                  FILTER_OPERATORS.some((entry) => entry.value === value)
                    ? value
                    : '$eq';
                onChange(
                  replaceAt(conditions, condition.id, {
                    operator,
                    value:
                      operator === '$isNull' && condition.value === ''
                        ? 'true'
                        : condition.value,
                  }),
                );
              }}
            >
              <SelectTrigger
                aria-label={`Operator for filter ${String(index + 1)}`}
                className="w-44"
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {FILTER_OPERATORS.map((operator) => {
                  const supported = isOperatorSupported(
                    operator.value,
                    attributeTypes.get(condition.attribute),
                  );
                  return (
                    <SelectItem
                      key={operator.value}
                      value={operator.value}
                      disabled={!supported}
                    >
                      {operator.label}
                    </SelectItem>
                  );
                })}
              </SelectContent>
            </Select>
            <Input
              aria-label={`Value for filter ${String(index + 1)}`}
              className="min-w-24 flex-1"
              value={condition.value}
              placeholder={condition.operator === '$in' ? 'a, b, c' : 'Value'}
              onChange={(event) => {
                onChange(
                  replaceAt(conditions, condition.id, {
                    value: event.target.value,
                  }),
                );
              }}
            />
            <IconButton
              aria-label={`Remove filter ${String(index + 1)}`}
              icon={<Trash2 />}
              onClick={() => {
                onChange(
                  conditions.filter((entry) => entry.id !== condition.id),
                );
              }}
            />
          </div>
        ))}
      </div>
      <div className="flex items-center justify-between gap-2">
        <Button
          type="button"
          variant="ghost"
          size="sm"
          onClick={() => {
            onChange([...conditions, createFilterCondition()]);
          }}
        >
          <Plus aria-hidden="true" />
          Add filter
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          disabled={conditions.length === 0}
          onClick={() => {
            onChange([]);
          }}
        >
          Clear filters
        </Button>
      </div>
    </div>
  );
}

export interface FilterButtonProps extends FilterPanelProps {
  /** Rendered next to the label; defaults to the active filter count. */
  count?: number;
}

/** "Filter (n)" button that opens the panel in a popover. */
export function FilterButton({
  count,
  conditions,
  className,
  ...panelProps
}: FilterButtonProps) {
  const activeCount =
    count ??
    conditions.filter(
      (condition) => condition.enabled && condition.attribute !== '',
    ).length;

  return (
    <Popover>
      <PopoverTrigger
        render={
          <Button type="button" variant="secondary" className={className} />
        }
      >
        <Filter aria-hidden="true" />
        Filter
        {activeCount > 0 ? (
          <Badge tone="accent" aria-hidden="true">
            {activeCount}
          </Badge>
        ) : null}
        <span className="sr-only">
          {activeCount > 0 ? `, ${String(activeCount)} active` : ''}
        </span>
      </PopoverTrigger>
      <PopoverContent className="w-[min(42rem,calc(100vw-2rem))]">
        <FilterPanel
          conditions={conditions}
          attributes={panelProps.attributes}
          onChange={panelProps.onChange}
        />
      </PopoverContent>
    </Popover>
  );
}
