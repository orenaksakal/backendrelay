import {
  MoreHorizontal,
  Pause,
  Play,
  Plus,
  RefreshCw,
  Users as UsersIcon,
} from 'lucide-react';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { AppDataGate } from '@/components/data/app-data-gate';
import { IdChip } from '@/components/data/id-chip';
import { RelativeTime } from '@/components/data/relative-time';
import { SearchInput } from '@/components/data/search-input';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { TypedConfirmDialog } from '@/components/feedback/typed-confirm-dialog';
import { PageHeader } from '@/components/layout/page-header';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { copyText } from '@/lib/clipboard';
import { formatCount } from '@/lib/format';
import { peekAppClient, useAppSchema } from '@/lib/instant';
import type { AppLink } from '@/lib/instant';
import { getRuntimeConfig } from '@/lib/runtime-config';
import { PAGE_ACTION_EVENT } from '@/lib/shortcuts';
import { showErrorToast, toastManager } from '@/lib/toast';
import {
  appAdminConfig,
  deleteUser,
  sendMagicCode,
  signOutUser,
} from '@/lib/users/admin';
import { useUsers } from '@/lib/users/rows';
import {
  MAX_USERS_PAGE_SIZE,
  userCreatedAt,
  userEmail,
  USERS_PAGE_SIZE,
  type UserRow,
} from '@/lib/users/users';
import {
  isUsersType,
  USERS_TYPE_LABELS,
  USERS_TYPES,
  usersSearchFor,
  type UsersSearch,
} from '@/lib/users/url-state';
import { CreateUserDialog } from './create-user-dialog';
import { UserSheet } from './user-sheet';

export interface UsersPageProps {
  app: App;
  role: EffectiveRole;
  search: UsersSearch;
  /** Route-owned URL write; every filter and the open sheet live there. */
  onSearchChange: (search: UsersSearch) => void;
}

const NO_LINKS: ReadonlyArray<AppLink> = [];

/** C.8 Users: the role gate and page header live outside the data gate. */
export function UsersPage({
  app,
  role,
  search,
  onSearchChange,
}: UsersPageProps) {
  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title="Users"
        description="Every end user in this app, read live."
      />
      <AppDataGate
        app={app}
        role={role}
        thing="users"
        forbiddenMessage="The user list needs admin access to this app."
      >
        <UsersWorkspace
          app={app}
          search={search}
          onSearchChange={onSearchChange}
        />
      </AppDataGate>
    </div>
  );
}

interface UsersWorkspaceProps {
  app: App;
  search: UsersSearch;
  onSearchChange: UsersPageProps['onSearchChange'];
}

function UsersWorkspace({ app, search, onSearchChange }: UsersWorkspaceProps) {
  const client = peekAppClient(app.id);
  const schema = useAppSchema(app.id);
  const toolbarRef = useRef<HTMLDivElement>(null);

  const links = useMemo(
    () =>
      schema.namespaces.find((namespace) => namespace.name === '$users')
        ?.links ?? NO_LINKS,
    [schema.namespaces],
  );

  const adminConfig = useMemo(
    () =>
      appAdminConfig({
        appId: app.id,
        adminToken: app.admin_token,
        apiBaseUrl: getRuntimeConfig()?.apiBaseUrl,
      }),
    [app.id, app.admin_token],
  );

  const [limit, setLimit] = useState(USERS_PAGE_SIZE);
  const users = useUsers({
    client: client ?? null,
    text: search.q ?? '',
    type: search.type,
    limit,
  });

  // A new filter or search starts at the first page.
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- a new filter or search starts at the first page
    setLimit(USERS_PAGE_SIZE);
  }, [search.q, search.type]);

  const [createOpen, setCreateOpen] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<UserRow | null>(null);
  const [signOutTarget, setSignOutTarget] = useState<UserRow | null>(null);
  const [busy, setBusy] = useState(false);

  const openUser = useCallback(
    (userId: string) => {
      onSearchChange(usersSearchFor({ ...search, user: userId }));
    },
    [onSearchChange, search],
  );

  const closeUser = useCallback(() => {
    onSearchChange(usersSearchFor({ ...search, user: null }));
  }, [onSearchChange, search]);

  const openCreate = useCallback(() => {
    setCreateOpen(true);
  }, []);

  // `/` focuses the search box, N opens Create user and Shift+R refreshes (B.7).
  useEffect(() => {
    const listener = (event: Event) => {
      const detail = (event as CustomEvent<{ action?: string } | undefined>)
        .detail;
      if (detail?.action === 'search') {
        const input = toolbarRef.current?.querySelector<HTMLInputElement>(
          'input[role="searchbox"]',
        );
        if (input === null || input === undefined) return;
        event.preventDefault();
        input.focus();
        return;
      }
      if (detail?.action === 'refresh') {
        event.preventDefault();
        users.refresh();
        return;
      }
      if (detail?.action === 'new') {
        event.preventDefault();
        openCreate();
      }
    };
    window.addEventListener(PAGE_ACTION_EVENT, listener);
    return () => {
      window.removeEventListener(PAGE_ACTION_EVENT, listener);
    };
  }, [openCreate, users.refresh]);

  function changeFilters(next: UsersSearch): void {
    onSearchChange(next);
  }

  function sendCode(user: UserRow): void {
    const email = userEmail(user);
    if (adminConfig === null || email === null) return;
    setBusy(true);
    void (async () => {
      try {
        await sendMagicCode(adminConfig, email);
        toastManager.add({
          type: 'success',
          title: `Magic code sent to ${email}`,
        });
      } catch (failure) {
        showErrorToast(failure);
      } finally {
        setBusy(false);
      }
    })();
  }

  async function confirmSignOut(): Promise<void> {
    if (signOutTarget === null || adminConfig === null) return;
    setBusy(true);
    try {
      await signOutUser(adminConfig, signOutTarget.id);
      toastManager.add({
        type: 'success',
        title: 'Signed out everywhere',
      });
      setSignOutTarget(null);
    } catch (failure) {
      showErrorToast(failure);
    } finally {
      setBusy(false);
    }
  }

  async function confirmDelete(): Promise<void> {
    if (deleteTarget === null || adminConfig === null) return;
    setBusy(true);
    try {
      await deleteUser(adminConfig, deleteTarget.id);
      toastManager.add({ type: 'success', title: 'User deleted' });
      if (search.user === deleteTarget.id) closeUser();
      setDeleteTarget(null);
    } catch (failure) {
      showErrorToast(failure);
    } finally {
      setBusy(false);
    }
  }

  if (client === undefined) return null;

  const filtering = search.q !== undefined || search.type !== 'all';
  const targetRow = users.users.find((user) => user.id === search.user) ?? null;
  const countLabel =
    users.users.length === 1
      ? '1 user loaded'
      : `${formatCount(users.users.length)} users loaded`;

  const columns: ReadonlyArray<SimpleTableColumn<UserRow>> = [
    {
      id: 'email',
      header: 'Email',
      cell: (user) => <UserEmailCell user={user} />,
    },
    {
      id: 'id',
      header: 'User ID',
      cell: (user) => <IdChip value={user.id} />,
    },
    {
      id: 'created',
      header: 'Created',
      cell: (user) => <UserCreatedCell user={user} />,
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      className: 'w-10',
      cell: (user) => (
        <UserRowMenu
          user={user}
          busy={busy}
          onOpen={() => {
            openUser(user.id);
          }}
          onSendCode={() => {
            sendCode(user);
          }}
          onSignOut={() => {
            setSignOutTarget(user);
          }}
          onDelete={() => {
            setDeleteTarget(user);
          }}
        />
      ),
    },
  ];

  return (
    <div className="flex flex-col gap-3">
      <div ref={toolbarRef} className="flex flex-wrap items-center gap-2">
        <SearchInput
          aria-label="Search by email"
          placeholder="Search by email…"
          value={search.q ?? ''}
          onValueChange={(value) => {
            changeFilters(usersSearchFor({ ...search, q: value }));
          }}
          showShortcut
          className="min-w-48 flex-1"
        />
        <Select
          value={search.type}
          onValueChange={(value) => {
            if (!isUsersType(value)) return;
            changeFilters(usersSearchFor({ ...search, type: value }));
          }}
        >
          <SelectTrigger aria-label="Filter by user type" className="w-36">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {USERS_TYPES.map((type) => (
              <SelectItem key={type} value={type}>
                {USERS_TYPE_LABELS[type]}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Button
          type="button"
          variant="secondary"
          aria-label={
            users.pending ? 'Refresh (new changes available)' : 'Refresh'
          }
          onClick={users.refresh}
        >
          <RefreshCw aria-hidden="true" />
          Refresh
          {users.pending ? (
            <span
              aria-hidden="true"
              className="bg-warning size-1.5 rounded-full"
            />
          ) : null}
        </Button>
        {users.paused ? (
          <Button type="button" variant="secondary" onClick={users.resume}>
            <Play aria-hidden="true" />
            Resume
          </Button>
        ) : (
          <Button
            type="button"
            variant="secondary"
            aria-pressed={true}
            onClick={users.togglePause}
          >
            <Pause aria-hidden="true" />
            Live
          </Button>
        )}
        <Button
          type="button"
          onClick={() => {
            openCreate();
          }}
        >
          <Plus aria-hidden="true" />
          Create user
        </Button>
      </div>

      {users.error !== null ? (
        <ErrorState
          thing="users"
          message={users.error}
          onRetry={users.refresh}
        />
      ) : users.isLoading && users.users.length === 0 ? (
        <TableSkeleton columns={5} />
      ) : users.users.length === 0 && !filtering ? (
        <EmptyState
          icon={UsersIcon}
          title="No users yet"
          description="Users appear here after their first sign-in, or create one with an email."
          action={
            <Button
              type="button"
              onClick={() => {
                openCreate();
              }}
            >
              <Plus aria-hidden="true" />
              Create user
            </Button>
          }
        />
      ) : (
        <>
          <SimpleTable
            caption="Users"
            columns={columns}
            rows={users.users}
            getRowId={(user) => user.id}
            onRowActivate={(user) => {
              openUser(user.id);
            }}
            empty={
              <span className="inline-flex flex-wrap items-center justify-center gap-1">
                No results for these filters.
                <Button
                  type="button"
                  variant="link"
                  size="sm"
                  onClick={() => {
                    changeFilters(usersSearchFor({ type: 'all' }));
                  }}
                >
                  Clear filters
                </Button>
              </span>
            }
          />

          <div className="flex flex-wrap items-center justify-between gap-2">
            <p className="text-muted-foreground text-xs">{countLabel}</p>
            {users.hasNextPage ? (
              <Button
                type="button"
                variant="secondary"
                size="sm"
                disabled={users.isLoading}
                onClick={() => {
                  setLimit((current) =>
                    Math.min(current + USERS_PAGE_SIZE, MAX_USERS_PAGE_SIZE),
                  );
                }}
              >
                Load more
              </Button>
            ) : null}
          </div>
        </>
      )}

      {users.volumePaused ? (
        <p role="status" className="text-warning text-xs">
          Live updates paused (high volume). Resume to follow them again.
        </p>
      ) : users.pending && !users.paused ? (
        <p role="status" className="sr-only">
          New changes available
        </p>
      ) : null}

      {search.user === undefined ? null : (
        <UserSheet
          key={search.user}
          appId={app.id}
          client={client}
          userId={search.user}
          links={links}
          fallback={targetRow}
          onOpenChange={(open) => {
            if (!open) closeUser();
          }}
          onSendMagicCode={sendCode}
          onSignOut={(user) => {
            setSignOutTarget(user);
          }}
          onDelete={(user) => {
            setDeleteTarget(user);
          }}
          busy={busy}
        />
      )}

      <CreateUserDialog
        open={createOpen}
        onOpenChange={setCreateOpen}
        config={adminConfig}
        onCreated={() => {
          users.refresh();
        }}
      />

      <ConfirmDialog
        open={signOutTarget !== null}
        onOpenChange={(open) => {
          if (!open) setSignOutTarget(null);
        }}
        title="Sign out this user everywhere?"
        description={
          signOutTarget === null ? (
            ''
          ) : (
            <>
              {userEmail(signOutTarget) ?? signOutTarget.id} is signed out of
              every session. They can sign in again with a new code.
            </>
          )
        }
        confirmLabel="Sign out everywhere"
        confirming={busy}
        onConfirm={() => {
          void confirmSignOut();
        }}
      />

      <TypedConfirmDialog
        open={deleteTarget !== null}
        onOpenChange={(open) => {
          if (!open) setDeleteTarget(null);
        }}
        title="Delete this user?"
        description="The user record is removed. Data the user wrote stays in the app."
        consequences={[
          'Every refresh token for this user stops working.',
          'The user can sign up again with the same email, but gets a new user ID.',
        ]}
        confirmText={
          deleteTarget === null
            ? ''
            : (userEmail(deleteTarget) ?? deleteTarget.id)
        }
        confirmLabel="Delete user"
        confirming={busy}
        onConfirm={() => {
          void confirmDelete();
        }}
      />
    </div>
  );
}

function UserEmailCell({ user }: { user: UserRow }) {
  const email = userEmail(user);
  if (email === null) {
    return (
      <span className="flex flex-wrap items-center gap-2">
        <Badge tone="neutral">Guest</Badge>
        <IdChip value={user.id} />
      </span>
    );
  }
  return <span className="text-cell break-all">{email}</span>;
}

function UserCreatedCell({ user }: { user: UserRow }) {
  const createdAt = userCreatedAt(user);
  if (createdAt === null) {
    return <span className="text-muted-foreground">—</span>;
  }
  return <RelativeTime value={createdAt} className="text-cell" />;
}

interface UserRowMenuProps {
  user: UserRow;
  busy: boolean;
  onOpen: () => void;
  onSendCode: () => void;
  onSignOut: () => void;
  onDelete: () => void;
}

function UserRowMenu({
  user,
  busy,
  onOpen,
  onSendCode,
  onSignOut,
  onDelete,
}: UserRowMenuProps) {
  const email = userEmail(user);
  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        render={
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="size-7 p-0"
            aria-label={`Actions for ${email ?? user.id}`}
          />
        }
      >
        <MoreHorizontal aria-hidden="true" />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuItem onClick={onOpen}>Open user</DropdownMenuItem>
        <DropdownMenuItem
          onClick={() => {
            void copyText(user.id).then((copied) => {
              toastManager.add({
                type: copied ? 'success' : 'error',
                title: copied ? 'User ID copied' : "Couldn't copy the user ID",
              });
            });
          }}
        >
          Copy user ID
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        {email === null ? null : (
          <DropdownMenuItem disabled={busy} onClick={onSendCode}>
            Send magic code
          </DropdownMenuItem>
        )}
        <DropdownMenuItem disabled={busy} onClick={onSignOut}>
          Sign out everywhere
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem
          className="text-destructive"
          disabled={busy}
          onClick={onDelete}
        >
          Delete user
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
