import { Columns3 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  columnLabel,
  isHideableColumn,
  type ExplorerColumn,
} from '@/lib/explorer/columns';

export interface ColumnsMenuProps {
  columns: ReadonlyArray<ExplorerColumn>;
  hidden: ReadonlyArray<string>;
  onToggle: (columnId: string) => void;
}

/**
 * "Columns ▾": show/hide one namespace's attribute and link columns (C.6).
 * The identity column stays visible so a row can always be recognised.
 */
export function ColumnsMenu({ columns, hidden, onToggle }: ColumnsMenuProps) {
  const hideable = columns.filter(isHideableColumn);
  const hiddenIds = new Set(hidden);

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        render={<Button type="button" variant="secondary" />}
        disabled={hideable.length === 0}
      >
        <Columns3 aria-hidden="true" />
        Columns
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-56">
        <DropdownMenuGroup>
          <DropdownMenuLabel>Columns</DropdownMenuLabel>
          <DropdownMenuSeparator />
          {hideable.map((column) => (
            <DropdownMenuCheckboxItem
              key={column.id}
              checked={!hiddenIds.has(column.id)}
              onCheckedChange={() => {
                onToggle(column.id);
              }}
            >
              <span className="truncate">{columnLabel(column)}</span>
            </DropdownMenuCheckboxItem>
          ))}
        </DropdownMenuGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
