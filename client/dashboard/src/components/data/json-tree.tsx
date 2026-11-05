import { ChevronRight } from 'lucide-react';
import { useState } from 'react';
import { cn } from '@/lib/utils';

/**
 * Read-only disclosure tree for JSON and query results (C.10/C.12). Branches
 * start expanded, every branch is a real button with `aria-expanded`, and leaf
 * values keep their JSON type visible.
 */

export interface JsonTreeProps {
  value: unknown;
  /** Accessible name of the tree, for example "Query result". */
  'aria-label': string;
  className?: string;
}

export function JsonTree({
  value,
  'aria-label': ariaLabel,
  className,
}: JsonTreeProps) {
  return (
    <div
      role="group"
      aria-label={ariaLabel}
      className={cn(
        'border-border bg-card text-cell overflow-auto rounded-md border p-2 font-mono',
        className,
      )}
    >
      <JsonNode name={undefined} value={value} />
    </div>
  );
}

interface JsonNodeProps {
  name: string | undefined;
  value: unknown;
}

function isBranch(
  value: unknown,
): value is Record<string, unknown> | unknown[] {
  return value !== null && typeof value === 'object';
}

function entriesOf(
  value: Record<string, unknown> | unknown[],
): Array<[string, unknown]> {
  return Array.isArray(value)
    ? value.map((item, index): [string, unknown] => [String(index), item])
    : Object.entries(value);
}

function JsonNode({ name, value }: JsonNodeProps) {
  const [open, setOpen] = useState(true);

  if (!isBranch(value)) {
    return <JsonLeaf name={name} value={value} />;
  }

  const entries = entriesOf(value);
  if (entries.length === 0) {
    return (
      <span className="inline-flex gap-1">
        {name === undefined ? null : (
          <span className="text-accent">{name}:</span>
        )}
        <span className="text-muted-foreground">
          {Array.isArray(value) ? '[]' : '{}'}
        </span>
      </span>
    );
  }

  return (
    <div>
      <button
        type="button"
        aria-expanded={open}
        className="hover:bg-muted -ml-1 inline-flex min-h-6 items-center gap-1 rounded-sm px-1 text-left"
        onClick={() => {
          setOpen((current) => !current);
        }}
      >
        <ChevronRight
          aria-hidden="true"
          className={cn(
            'text-muted-foreground size-3.5 transition-transform',
            open && 'rotate-90',
          )}
        />
        {name === undefined ? null : (
          <span className="text-accent">{name}:</span>
        )}
        <span className="text-muted-foreground">
          {Array.isArray(value)
            ? `[${String(entries.length)}]`
            : `{${String(entries.length)}}`}
        </span>
      </button>
      {open ? (
        <ul className="border-border ml-2 border-l pl-2">
          {entries.map(([key, child]) => (
            <li key={key}>
              <JsonNode name={key} value={child} />
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

function JsonLeaf({
  name,
  value,
}: {
  name: string | undefined;
  value: unknown;
}) {
  return (
    <span className="inline-flex min-w-0 gap-1">
      {name === undefined ? null : <span className="text-accent">{name}:</span>}
      <span className={leafClass(value)}>{leafText(value)}</span>
    </span>
  );
}

function leafClass(value: unknown): string {
  if (value === null || value === undefined) {
    return 'text-muted-foreground italic';
  }
  if (typeof value === 'string') return 'break-all';
  if (typeof value === 'number') return 'tabular-nums';
  return '';
}

function leafText(value: unknown): string {
  if (value === null) return 'null';
  if (value === undefined) return 'undefined';
  if (typeof value === 'string') return JSON.stringify(value);
  if (typeof value === 'boolean') return value ? 'true' : 'false';
  if (typeof value === 'bigint') return `${value.toString()}n`;
  return JSON.stringify(value);
}
