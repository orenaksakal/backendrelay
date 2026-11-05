import {
  Combobox,
  ComboboxButton,
  ComboboxInput,
  ComboboxOption,
  ComboboxOptions,
} from '@headlessui/react';
import { ChevronDownIcon, StarIcon } from '@heroicons/react/24/solid';
import { StarIcon as StarOutlineIcon } from '@heroicons/react/24/outline';
import { useRef, useState } from 'react';
import Link from '@/lib/navigation';
import { titleComparator } from '@/lib/app';
import { getPinnedAppIds, togglePinnedApp } from '@/lib/pinnedApps';
import type { InstantApp } from '@/lib/types';
import { cn, type TabItem } from '@/components/ui';

// Keep the original Instant dashboard destinations in their original order.
export const originalTabIds = [
  'home',
  'explorer',
  'schema',
  'perms',
  'auth',
  'webhooks',
  'backups',
  'repl',
  'sandbox',
  'admin',
  'oauth-apps',
] as const;

export function OriginalNavigation({
  tab,
  items,
  href,
  onNavigate,
}: {
  tab: string;
  items: TabItem[];
  href: (tab: string) => string;
  onNavigate: (tab: string) => void;
}) {
  const primary = originalTabIds.flatMap((id) =>
    items.filter((item) => item.id === id),
  );
  const link = (item: TabItem) => (
    <Link
      key={item.id}
      href={href(item.id)}
      aria-current={tab === item.id ? 'page' : undefined}
      className={cn(
        'dashboard-nav-item flex shrink-0 items-center gap-2 rounded px-4 py-0.5 text-sm whitespace-nowrap hover:bg-gray-100 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-indigo-500 md:rounded-none md:px-3 md:py-2 dark:hover:bg-neutral-600 md:dark:hover:bg-neutral-700/80',
        tab === item.id &&
          'bg-gray-200 dark:bg-neutral-700 md:dark:bg-neutral-600/50',
      )}
      onClick={(event) => {
        if (
          event.button !== 0 ||
          event.metaKey ||
          event.ctrlKey ||
          event.shiftKey ||
          event.altKey
        )
          return;
        event.preventDefault();
        onNavigate(item.id);
      }}
    >
      {item.icon && <span className="hidden md:inline">{item.icon}</span>}
      <span>{item.label}</span>
    </Link>
  );
  return (
    <nav
      aria-label="Application navigation"
      className="shrink-0 overflow-auto border-b border-gray-300 bg-gray-50 px-2 py-1 md:w-48 md:border-r md:border-b-0 md:p-0 dark:border-neutral-700/80 dark:bg-neutral-800/40"
    >
      <div className="flex h-full gap-0.5 overflow-auto bg-gray-50 md:flex-col md:gap-0 dark:bg-neutral-800/40">
        {primary.map(link)}
      </div>
    </nav>
  );
}

export function OriginalAppPicker({
  apps,
  appId,
  workspaceId,
  onSelect,
}: {
  apps: InstantApp[];
  appId: string;
  workspaceId: string;
  onSelect: (app: InstantApp) => void;
}) {
  const [query, setQuery] = useState('');
  const [pins, setPins] = useState(() => getPinnedAppIds(workspaceId));
  const inputRef = useRef<HTMLInputElement>(null);
  const current = apps.find((app) => app.id === appId) ?? null;
  const filtered = apps.filter((app) =>
    app.title.toLowerCase().includes(query.toLowerCase()),
  );
  const sorted = filtered.toSorted(
    (a, b) =>
      Number(pins.has(b.id)) - Number(pins.has(a.id)) || titleComparator(a, b),
  );
  const firstUnpinnedIndex = sorted.findIndex((app) => !pins.has(app.id));
  const hasPinnedAndUnpinned = firstUnpinnedIndex > 0;
  return (
    <Combobox
      immediate
      value={current}
      onChange={(app) => {
        if (app) onSelect(app);
        setQuery('');
        inputRef.current?.blur();
      }}
      onClose={() => setQuery('')}
    >
      <div className="relative min-w-0">
        <ComboboxInput
          ref={inputRef}
          aria-label="Switch application"
          className="w-full min-w-0! basis-[35%] truncate rounded-xs border-gray-300 py-1 pr-8 pl-3 text-sm/6 ring-0 focus:outline-hidden data-focus:outline-2 data-focus:-outline-offset-2 data-focus:outline-white/25 md:w-full md:basis-full dark:border-neutral-700 dark:bg-neutral-700/40"
          displayValue={(app: InstantApp | null) => app?.title ?? ''}
          onChange={(event) => setQuery(event.target.value)}
        />
        <ComboboxButton
          aria-label="Show applications"
          className="group absolute inset-y-0 right-0 px-2.5"
        >
          <ChevronDownIcon
            height="1em"
            className="fill-gray/300 group-data-hover:fill-gray"
          />
        </ComboboxButton>
      </div>
      <ComboboxOptions
        anchor="bottom"
        transition
        className="z-50 mx-2 my-1 max-h-80 overflow-auto border border-gray-300 bg-white shadow-lg transition duration-100 ease-in [--anchor-gap:var(--spacing-1)] empty:invisible data-leave:data-closed:opacity-0 md:min-w-(--input-width) dark:divide-neutral-600 dark:border-neutral-700 dark:bg-neutral-800 dark:text-white"
      >
        {sorted.map((app, index) => (
          <div key={app.id}>
            {hasPinnedAndUnpinned && index === firstUnpinnedIndex && (
              <div className="border-t border-gray-200 dark:border-neutral-600" />
            )}
            <ComboboxOption
              value={app}
              className="group/item cursor-pointer px-3 py-1 data-focus:bg-gray-100 dark:data-focus:bg-neutral-700/80"
            >
              <div className="flex items-center justify-between gap-2">
                <span className="truncate">{app.title}</span>
                <button
                  type="button"
                  aria-label={`${pins.has(app.id) ? 'Unpin' : 'Pin'} ${app.title}`}
                  className={cn(
                    'flex-none',
                    pins.has(app.id)
                      ? 'text-yellow-500 dark:text-yellow-400'
                      : 'text-gray-300 opacity-0 group-data-focus/item:opacity-100 focus-visible:opacity-100 dark:text-neutral-600',
                  )}
                  onPointerDown={(event) => {
                    // Headless UI selects the option on mouse-down. Keep pinning
                    // separate from switching applications, as in the original UI.
                    event.preventDefault();
                    event.stopPropagation();
                    togglePinnedApp(app.id, workspaceId);
                    setPins(getPinnedAppIds(workspaceId));
                  }}
                  onClick={(event) => {
                    event.preventDefault();
                    event.stopPropagation();
                    if (event.detail === 0) {
                      togglePinnedApp(app.id, workspaceId);
                      setPins(getPinnedAppIds(workspaceId));
                    }
                  }}
                >
                  {pins.has(app.id) ? (
                    <StarIcon className="h-3.5 w-3.5" />
                  ) : (
                    <StarOutlineIcon className="h-3.5 w-3.5" />
                  )}
                </button>
              </div>
            </ComboboxOption>
          </div>
        ))}
        {sorted.length === 0 && (
          <div className="px-3 py-2 text-gray-500">No applications found.</div>
        )}
      </ComboboxOptions>
    </Combobox>
  );
}
