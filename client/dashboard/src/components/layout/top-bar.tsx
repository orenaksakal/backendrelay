import { Link, useRouter, type NavigateOptions } from '@tanstack/react-router';
import {
  ChevronDown,
  Diamond,
  Keyboard,
  Menu,
  Monitor,
  Moon,
  Search,
  Sun,
} from 'lucide-react';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { StatusPill } from '@/components/feedback/status-pill';
import { Button } from '@/components/ui/button';
import { IconButton } from '@/components/layout/icon-button';
import { Kbd } from '@/components/ui/kbd';
import { signOut, useSession } from '@/lib/auth/session';
import { isTheme, useTheme, type Theme } from '@/lib/theme';
import { formatShortcut } from '@/lib/shortcuts';
import {
  activeNavItem,
  pageLabelFromPath,
  type Navigation,
} from '@/lib/navigation';
import type { App } from '@/lib/api/endpoints/schemas';
import type { PaletteMode } from '@/components/layout/command-palette';
import type { Workspace } from '@/lib/workspace/workspaces';

export interface TopBarProps {
  navigation: Navigation | null;
  pathname: string;
  app?: App;
  workspace?: Workspace;
  superuser: boolean;
  menuOpen: boolean;
  onOpenMenu: () => void;
  /** Opens the palette in picker mode from a breadcrumb switcher (B.6). */
  onOpenPalette: (mode: PaletteMode) => void;
  onOpenShortcuts: () => void;
}

/**
 * 48px top bar: mobile menu button, logo and the breadcrumb switchers on the
 * left; search, theme and avatar menus on the right (B.6). The breadcrumb
 * segments and the search button open the command palette (B.7).
 */
export function TopBar({
  navigation,
  pathname,
  app,
  workspace,
  superuser,
  menuOpen,
  onOpenMenu,
  onOpenPalette,
  onOpenShortcuts,
}: TopBarProps) {
  const active = navigation ? activeNavItem(navigation, pathname) : undefined;
  const section = active?.label ?? pageLabelFromPath(pathname);
  const status = app?.effective_status ?? app?.status;

  return (
    <header className="bg-sidebar border-border sticky top-0 z-(--z-sticky) flex h-12 shrink-0 items-center gap-1 border-b px-2 sm:px-3">
      <IconButton
        aria-label="Open navigation"
        aria-haspopup="dialog"
        aria-expanded={menuOpen}
        icon={<Menu aria-hidden="true" />}
        className={navigation === null ? 'hidden' : 'lg:hidden'}
        onClick={onOpenMenu}
      />
      <Link
        to="/"
        aria-label="BackendRelay home"
        className="text-foreground hover:bg-muted inline-flex h-8 shrink-0 items-center gap-2 rounded-md px-1.5 font-semibold"
      >
        <Diamond aria-hidden="true" className="size-4" />
        <span className="hidden sm:inline">BackendRelay</span>
      </Link>

      <nav aria-label="Breadcrumb" className="flex min-w-0 items-center gap-1">
        {workspace === undefined ? null : (
          <>
            <BreadcrumbSeparator />
            <Button
              type="button"
              variant="ghost"
              size="md"
              aria-label={`Switch workspace, current ${workspace.title}`}
              aria-haspopup="dialog"
              className="max-w-[160px] gap-1 px-1.5"
              onClick={() => {
                onOpenPalette('workspaces');
              }}
            >
              <span className="truncate">{workspace.title}</span>
              <ChevronDown aria-hidden="true" />
            </Button>
          </>
        )}

        {app === undefined ? null : (
          <>
            <BreadcrumbSeparator />
            <Button
              type="button"
              variant="ghost"
              size="md"
              aria-label={`Switch app, current ${app.title}`}
              aria-haspopup="dialog"
              className="max-w-[200px] gap-1 px-1.5"
              onClick={() => {
                onOpenPalette('apps');
              }}
            >
              <span className="truncate">{app.title}</span>
              <ChevronDown aria-hidden="true" />
            </Button>
            {status !== undefined && status !== 'active' ? (
              <StatusPill status={status} />
            ) : null}
          </>
        )}

        <BreadcrumbSeparator />
        <span
          aria-current="page"
          className="text-foreground truncate px-1 text-sm font-medium"
        >
          {section}
        </span>
      </nav>

      <div className="ml-auto flex shrink-0 items-center gap-1">
        <Button
          type="button"
          variant="ghost"
          size="md"
          aria-keyshortcuts="Meta+K Control+K"
          className="text-muted-foreground gap-2"
          onClick={() => {
            onOpenPalette('all');
          }}
        >
          <Search aria-hidden="true" />
          <span className="hidden sm:inline">Search…</span>
          <Kbd className="hidden md:inline-flex">{formatShortcut('mod+k')}</Kbd>
        </Button>
        <ThemeMenu />
        <AccountMenu superuser={superuser} onOpenShortcuts={onOpenShortcuts} />
      </div>
    </header>
  );
}

function BreadcrumbSeparator() {
  return (
    <span aria-hidden="true" className="text-muted-foreground text-xs">
      /
    </span>
  );
}

const THEME_LABELS: Record<Theme, string> = {
  system: 'System',
  light: 'Light',
  dark: 'Dark',
};

function ThemeMenu() {
  const { theme, setTheme } = useTheme();
  const Icon = theme === 'dark' ? Moon : theme === 'light' ? Sun : Monitor;

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        render={
          <Button
            type="button"
            variant="ghost"
            size="md"
            aria-label={`Theme: ${THEME_LABELS[theme]}`}
            className="size-8 p-0"
          />
        }
      >
        <Icon aria-hidden="true" />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuRadioGroup
          value={theme}
          onValueChange={(value) => {
            if (isTheme(value)) setTheme(value);
          }}
        >
          <DropdownMenuLabel>Theme</DropdownMenuLabel>
          <DropdownMenuRadioItem value="system">
            <Monitor aria-hidden="true" />
            System
          </DropdownMenuRadioItem>
          <DropdownMenuRadioItem value="light">
            <Sun aria-hidden="true" />
            Light
          </DropdownMenuRadioItem>
          <DropdownMenuRadioItem value="dark">
            <Moon aria-hidden="true" />
            Dark
          </DropdownMenuRadioItem>
        </DropdownMenuRadioGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

interface AccountMenuProps {
  superuser: boolean;
  onOpenShortcuts: () => void;
}

function AccountMenu({ superuser, onOpenShortcuts }: AccountMenuProps) {
  const router = useRouter();
  const session = useSession();
  const email = session?.user.email ?? 'Account';
  const initial = email.charAt(0).toUpperCase();

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        render={
          <Button
            type="button"
            variant="ghost"
            size="md"
            aria-label="Account menu"
            className="size-8 rounded-full p-0"
          />
        }
      >
        <span
          aria-hidden="true"
          className="bg-primary text-primary-foreground grid size-6 place-items-center rounded-full text-xs font-medium"
        >
          {initial}
        </span>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="min-w-56">
        <DropdownMenuGroup>
          <DropdownMenuLabel className="truncate">{email}</DropdownMenuLabel>
          <DropdownMenuSeparator />
          <DropdownMenuItem
            onClick={() => {
              void router.navigate({
                href: '/account/profile',
              } as NavigateOptions);
            }}
          >
            Account
          </DropdownMenuItem>
          {superuser ? (
            <DropdownMenuItem
              onClick={() => {
                void router.navigate({
                  href: '/operator/settings',
                } as NavigateOptions);
              }}
            >
              Operator
            </DropdownMenuItem>
          ) : null}
          <DropdownMenuItem
            onClick={() => {
              onOpenShortcuts();
            }}
          >
            <Keyboard aria-hidden="true" />
            Keyboard shortcuts
            <Kbd className="ml-auto">?</Kbd>
          </DropdownMenuItem>
          <DropdownMenuSeparator />
          <DropdownMenuItem
            onClick={() => {
              signOut();
              void router.navigate({ href: '/login' } as NavigateOptions);
            }}
          >
            Sign out
          </DropdownMenuItem>
        </DropdownMenuGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
