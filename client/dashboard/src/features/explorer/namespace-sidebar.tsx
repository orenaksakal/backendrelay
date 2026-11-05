import { ChevronRight, MoreHorizontal, Plus, Star, Table2 } from 'lucide-react';
import {
  useRef,
  useState,
  type KeyboardEvent,
  type KeyboardEventHandler,
  type ReactNode,
} from 'react';
import { SearchInput } from '@/components/data/search-input';
import { IconButton } from '@/components/layout/icon-button';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { cn } from '@/lib/utils';
import { namespaceCountLabel } from '@/lib/explorer/entities';
import type { AppNamespace } from '@/lib/instant';

export interface NamespaceSidebarProps {
  namespaces: ReadonlyArray<AppNamespace>;
  /** Aggregate counts from the app client; missing namespaces stay blank. */
  counts: Record<string, number>;
  /** Namespaces whose effective write rule allows everyone (C.6 warning dot). */
  publicWrite: ReadonlyArray<string>;
  selected: string | null;
  pins: ReadonlyArray<string>;
  isLoading: boolean;
  onSelect: (namespace: string) => void;
  onTogglePin: (namespace: string) => void;
  onNewNamespace: () => void;
  /**
   * C.7 adds a per-row overflow menu. The Explorer leaves it out because a
   * namespace delete is a schema change, not a data edit.
   */
  onDeleteNamespace?: (namespace: string) => void;
}

const NAVIGATION_KEYS = new Set(['ArrowDown', 'ArrowUp', 'Home', 'End']);

/**
 * 256px namespace rail (C.6): pinned namespaces, user namespaces and a
 * collapsed system group, with a find box. Arrow keys walk the rendered rows
 * and Enter selects the focused one; pinning is a separate button so it never
 * changes the selection.
 */
export function NamespaceSidebar({
  namespaces,
  counts,
  publicWrite,
  selected,
  pins,
  isLoading,
  onSelect,
  onTogglePin,
  onNewNamespace,
  onDeleteNamespace,
}: NamespaceSidebarProps) {
  const [query, setQuery] = useState('');
  const [systemOpen, setSystemOpen] = useState(false);
  const listRef = useRef<HTMLDivElement>(null);
  const needle = query.trim().toLowerCase();
  const searching = needle !== '';
  const warning = new Set(publicWrite);

  const filtered = namespaces.filter((candidate) =>
    candidate.name.toLowerCase().includes(needle),
  );
  const pinnedNames = new Set(pins);
  const pinned = filtered.filter((candidate) =>
    pinnedNames.has(candidate.name),
  );
  const user = filtered.filter(
    (candidate) => !candidate.isSystem && !pinnedNames.has(candidate.name),
  );
  const system = filtered.filter(
    (candidate) => candidate.isSystem && !pinnedNames.has(candidate.name),
  );
  const showSystem = searching || systemOpen;

  function handleKeyDown(event: KeyboardEvent<HTMLButtonElement>) {
    if (!NAVIGATION_KEYS.has(event.key)) return;
    const rows = [
      ...(listRef.current?.querySelectorAll<HTMLButtonElement>(
        'button[data-namespace]',
      ) ?? []),
    ];
    if (rows.length === 0) return;
    const index = rows.indexOf(document.activeElement as HTMLButtonElement);
    let next = index;
    if (event.key === 'ArrowDown') next = index < 0 ? 0 : index + 1;
    if (event.key === 'ArrowUp') next = index <= 0 ? 0 : index - 1;
    if (event.key === 'Home') next = 0;
    if (event.key === 'End') next = rows.length - 1;
    const target = rows[Math.min(Math.max(next, 0), rows.length - 1)];
    if (target === undefined) return;
    event.preventDefault();
    target.focus();
  }

  return (
    <aside
      aria-label="Namespaces"
      className="border-border bg-card flex w-full min-w-0 flex-col gap-2 rounded-lg border p-2"
    >
      <SearchInput
        aria-label="Find namespace"
        placeholder="Find namespace"
        value={query}
        onValueChange={setQuery}
      />

      <div
        ref={listRef}
        className="flex min-h-0 flex-col gap-2 overflow-y-auto"
      >
        {isLoading ? (
          <p className="text-muted-foreground px-2 py-1 text-xs">
            Loading namespaces…
          </p>
        ) : null}

        {!isLoading && filtered.length === 0 ? (
          <p className="text-muted-foreground px-2 py-1 text-xs">
            No namespaces match “{query.trim()}”.
          </p>
        ) : null}

        {pinned.length > 0 ? (
          <NamespaceGroup label="Pinned">
            {pinned.map((namespace) => (
              <NamespaceRow
                key={`pinned-${namespace.name}`}
                namespace={namespace}
                count={counts[namespace.name]}
                warning={warning.has(namespace.name)}
                selected={selected === namespace.name}
                pinned
                onSelect={onSelect}
                onTogglePin={onTogglePin}
                onKeyDown={handleKeyDown}
                onDeleteNamespace={onDeleteNamespace}
              />
            ))}
          </NamespaceGroup>
        ) : null}

        {user.length > 0 ? (
          <NamespaceGroup label="Namespaces">
            {user.map((namespace) => (
              <NamespaceRow
                key={namespace.name}
                namespace={namespace}
                count={counts[namespace.name]}
                warning={warning.has(namespace.name)}
                selected={selected === namespace.name}
                pinned={false}
                onSelect={onSelect}
                onTogglePin={onTogglePin}
                onKeyDown={handleKeyDown}
                onDeleteNamespace={onDeleteNamespace}
              />
            ))}
          </NamespaceGroup>
        ) : null}

        {system.length > 0 ? (
          <div className="flex flex-col gap-1">
            <button
              type="button"
              aria-expanded={showSystem}
              className="text-muted-foreground hover:text-foreground flex h-6 items-center gap-1 px-2 text-[11px] font-medium tracking-[0.04em] uppercase"
              onClick={() => {
                setSystemOpen((current) => !current);
              }}
            >
              <ChevronRight
                aria-hidden="true"
                className={cn(
                  'size-3 transition-transform',
                  showSystem && 'rotate-90',
                )}
              />
              System
            </button>
            {showSystem ? (
              <ul className="flex flex-col gap-0.5">
                {system.map((namespace) => (
                  <NamespaceRow
                    key={namespace.name}
                    namespace={namespace}
                    count={counts[namespace.name]}
                    warning={warning.has(namespace.name)}
                    selected={selected === namespace.name}
                    pinned={pinnedNames.has(namespace.name)}
                    onSelect={onSelect}
                    onTogglePin={onTogglePin}
                    onKeyDown={handleKeyDown}
                    onDeleteNamespace={onDeleteNamespace}
                  />
                ))}
              </ul>
            ) : null}
          </div>
        ) : null}
      </div>

      <div className="border-border mt-auto border-t pt-2">
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="w-full justify-start"
          onClick={onNewNamespace}
        >
          <Plus aria-hidden="true" />
          New namespace
        </Button>
      </div>
    </aside>
  );
}

function NamespaceGroup({
  label,
  children,
}: {
  label: string;
  children: ReactNode;
}) {
  return (
    <div className="flex flex-col gap-1">
      <p
        aria-hidden="true"
        className="text-muted-foreground px-2 text-[11px] font-medium tracking-[0.04em] uppercase"
      >
        {label}
      </p>
      <ul className="flex flex-col gap-0.5">{children}</ul>
    </div>
  );
}

interface NamespaceRowProps {
  namespace: AppNamespace;
  count: number | undefined;
  warning: boolean;
  selected: boolean;
  pinned: boolean;
  onSelect: (namespace: string) => void;
  onTogglePin: (namespace: string) => void;
  onKeyDown: KeyboardEventHandler<HTMLButtonElement>;
  onDeleteNamespace?: (namespace: string) => void;
}

function NamespaceRow({
  namespace,
  count,
  warning,
  selected,
  pinned,
  onSelect,
  onTogglePin,
  onKeyDown,
  onDeleteNamespace,
}: NamespaceRowProps) {
  const countText = namespaceCountLabel(count);
  return (
    <li className="flex items-center gap-0.5">
      <button
        type="button"
        data-namespace={namespace.name}
        aria-current={selected ? 'true' : undefined}
        onKeyDown={onKeyDown}
        onClick={() => {
          onSelect(namespace.name);
        }}
        className={cn(
          'flex h-8 min-w-0 flex-1 items-center gap-2 rounded-md px-2 text-left text-sm transition-colors',
          selected
            ? 'bg-muted text-foreground font-medium'
            : 'text-muted-foreground hover:bg-muted hover:text-foreground',
        )}
      >
        <Table2 aria-hidden="true" className="size-3.5 shrink-0" />
        <span className="min-w-0 flex-1 truncate">{namespace.name}</span>
        {warning ? (
          <span
            role="img"
            aria-label="Anyone can write to this namespace"
            title="Anyone can write to this namespace"
            className="bg-warning size-1.5 shrink-0 rounded-full"
          />
        ) : null}
        {countText === '' ? null : (
          <span className="text-muted-foreground shrink-0 text-xs tabular-nums">
            {countText}
          </span>
        )}
      </button>
      <IconButton
        aria-label={
          pinned ? `Unpin ${namespace.name}` : `Pin ${namespace.name}`
        }
        size="sm"
        icon={
          <Star
            aria-hidden="true"
            className={cn('size-3.5', pinned && 'text-warning fill-current')}
          />
        }
        onClick={() => {
          onTogglePin(namespace.name);
        }}
      />
      {onDeleteNamespace === undefined || namespace.isSystem ? null : (
        <DropdownMenu>
          <DropdownMenuTrigger
            render={
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="size-7 shrink-0 p-0"
                aria-label={`More actions for ${namespace.name}`}
              />
            }
          >
            <MoreHorizontal aria-hidden="true" />
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem
              className="text-destructive"
              onClick={() => {
                onDeleteNamespace(namespace.name);
              }}
            >
              Delete namespace
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      )}
    </li>
  );
}
