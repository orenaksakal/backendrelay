import { Dialog as DialogPrimitive } from '@base-ui/react/dialog';
import { useRouter, type NavigateOptions } from '@tanstack/react-router';
import { Diamond, PanelLeftClose, PanelLeftOpen, X } from 'lucide-react';
import type * as React from 'react';
import { Button } from '@/components/ui/button';
import { IconButton } from '@/components/layout/icon-button';
import { Kbd } from '@/components/ui/kbd';
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from '@/components/ui/tooltip';
import {
  navLandmarkLabel,
  type NavItem,
  type Navigation,
} from '@/lib/navigation';
import { sidebarWidth, type SidebarState } from '@/lib/sidebar';
import { cn } from '@/lib/utils';

export interface SidebarNavProps {
  navigation: Navigation;
  pathname: string;
  collapsed?: boolean;
  onNavigate?: () => void;
}

/**
 * Sidebar navigation shared by the desktop rail and the mobile drawer. Group
 * labels are hidden from assistive tech; the nav landmark names the area
 * (B.6).
 */
export function SidebarNav({
  navigation,
  pathname,
  collapsed = false,
  onNavigate,
}: SidebarNavProps) {
  return (
    <nav
      aria-label={navLandmarkLabel(navigation.area)}
      className={cn('flex flex-col gap-1 px-2 py-2', collapsed && 'px-1.5')}
    >
      {navigation.groups.map((group) => (
        <div key={group.id} className="flex flex-col gap-0.5">
          {group.label === undefined ? null : (
            <div
              aria-hidden="true"
              className={cn(
                'text-muted-foreground px-2 pt-3 pb-1 text-[11px] font-medium tracking-[0.04em] uppercase',
                collapsed && 'sr-only',
              )}
            >
              {group.label}
            </div>
          )}
          <ul className="flex flex-col gap-0.5">
            {group.items.map((item) => (
              <li key={item.id}>
                <SidebarItemLink
                  item={item}
                  pathname={pathname}
                  collapsed={collapsed}
                  onNavigate={onNavigate}
                />
              </li>
            ))}
          </ul>
        </div>
      ))}
    </nav>
  );
}

interface SidebarItemLinkProps {
  item: NavItem;
  pathname: string;
  collapsed: boolean;
  onNavigate?: () => void;
}

function SidebarItemLink({
  item,
  pathname,
  collapsed,
  onNavigate,
}: SidebarItemLinkProps) {
  const router = useRouter();
  const active = isActive(item, pathname);
  const Icon = item.icon;

  const anchor = (
    <a
      href={item.href}
      aria-label={collapsed ? item.label : undefined}
      aria-current={active ? 'page' : undefined}
      onClick={(event) => {
        if (isModifiedClick(event)) return;
        event.preventDefault();
        onNavigate?.();
        // Most screens land in later steps; `href` is the documented way to
        // navigate to a fully built path the route table cannot type yet.
        void router.navigate({ href: item.href } as NavigateOptions);
      }}
      className={cn(
        'relative flex h-8 items-center gap-2 rounded-md text-sm transition-colors',
        collapsed ? 'justify-center px-0' : 'px-2',
        active
          ? 'text-accent font-medium'
          : 'text-muted-foreground hover:bg-muted hover:text-foreground',
      )}
    >
      {active ? (
        <span
          aria-hidden="true"
          className="bg-accent absolute inset-y-1 left-0 w-0.5 rounded-full"
        />
      ) : null}
      <Icon aria-hidden="true" className="size-4 shrink-0" />
      {collapsed ? null : <span className="truncate">{item.label}</span>}
    </a>
  );

  if (!collapsed) return anchor;

  return (
    <Tooltip>
      <TooltipTrigger render={anchor} />
      <TooltipContent side="right">{item.label}</TooltipContent>
    </Tooltip>
  );
}

function isActive(item: NavItem, pathname: string): boolean {
  const target = item.href.endsWith('/') ? item.href.slice(0, -1) : item.href;
  const current = pathname.endsWith('/') ? pathname.slice(0, -1) : pathname;
  return item.exact
    ? current === target
    : current === target || current.startsWith(`${target}/`);
}

function isModifiedClick(event: React.MouseEvent<HTMLAnchorElement>): boolean {
  return event.metaKey || event.ctrlKey || event.shiftKey || event.altKey;
}

export interface SidebarProps extends SidebarNavProps {
  state: SidebarState;
  onToggle: () => void;
}

/** Desktop rail: 224px expanded, 56px collapsed, with the width transition. */
export function Sidebar({
  state,
  onToggle,
  navigation,
  pathname,
  onNavigate,
}: SidebarProps) {
  const collapsed = state === 'collapsed';

  return (
    <aside
      style={{ width: sidebarWidth(state) }}
      data-state={state}
      className="bg-sidebar border-border hidden shrink-0 flex-col border-r transition-[width] duration-150 ease-out lg:flex"
    >
      <div className="min-h-0 flex-1 overflow-y-auto">
        <SidebarNav
          navigation={navigation}
          pathname={pathname}
          collapsed={collapsed}
          onNavigate={onNavigate}
        />
      </div>
      <div className="border-border border-t p-2">
        {collapsed ? (
          <IconButton
            aria-label="Expand sidebar"
            icon={<PanelLeftOpen aria-hidden="true" />}
            shortcut="["
            className="w-full"
            onClick={onToggle}
          />
        ) : (
          <Tooltip>
            <TooltipTrigger
              render={
                <Button
                  type="button"
                  variant="ghost"
                  size="md"
                  aria-label="Collapse sidebar"
                  className="w-full justify-start gap-2"
                  onClick={onToggle}
                />
              }
            >
              <PanelLeftClose aria-hidden="true" />
              <span>Collapse</span>
            </TooltipTrigger>
            <TooltipContent>
              Collapse sidebar
              <Kbd className="ml-1.5">[</Kbd>
            </TooltipContent>
          </Tooltip>
        )}
      </div>
    </aside>
  );
}

export interface SidebarDrawerProps extends SidebarNavProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/** Below `lg` the sidebar becomes a left drawer opened from the top bar (B.3). */
export function SidebarDrawer({
  open,
  onOpenChange,
  navigation,
  pathname,
  onNavigate,
}: SidebarDrawerProps) {
  return (
    <DialogPrimitive.Root open={open} onOpenChange={onOpenChange}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Backdrop
          data-slot="sidebar-backdrop"
          className="bg-foreground/40 fixed inset-0 z-(--z-drawer) transition-opacity duration-200 ease-out data-[ending-style]:opacity-0 data-[starting-style]:opacity-0"
        />
        <DialogPrimitive.Popup
          data-slot="sidebar-drawer"
          className="border-border bg-sidebar text-foreground fixed inset-y-0 left-0 z-(--z-drawer) flex w-[224px] flex-col border-r shadow-lg transition-transform duration-200 ease-out data-[ending-style]:-translate-x-full data-[starting-style]:-translate-x-full"
        >
          <div className="border-border flex h-12 shrink-0 items-center justify-between border-b px-3">
            <DialogPrimitive.Title className="flex items-center gap-2 text-sm font-semibold">
              <Diamond aria-hidden="true" className="size-4" />
              BackendRelay
            </DialogPrimitive.Title>
            <DialogPrimitive.Close
              aria-label="Close navigation"
              className="text-muted-foreground hover:bg-muted hover:text-foreground inline-flex size-7 items-center justify-center rounded-md transition-colors"
            >
              <X aria-hidden="true" className="size-4" />
            </DialogPrimitive.Close>
          </div>
          <DialogPrimitive.Description className="sr-only">
            Dashboard sections
          </DialogPrimitive.Description>
          <div className="min-h-0 flex-1 overflow-y-auto">
            <SidebarNav
              navigation={navigation}
              pathname={pathname}
              onNavigate={() => {
                onOpenChange(false);
                onNavigate?.();
              }}
            />
          </div>
        </DialogPrimitive.Popup>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}
