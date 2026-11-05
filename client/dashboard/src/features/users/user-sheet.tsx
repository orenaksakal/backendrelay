import { Link } from '@tanstack/react-router';
import { Users } from 'lucide-react';
import { useMemo, useState } from 'react';
import { CodeEditor } from '@/components/code-editor/code-editor';
import { CopyButton } from '@/components/data/copy-button';
import { IdChip } from '@/components/data/id-chip';
import {
  KeyValueList,
  type KeyValueItem,
} from '@/components/data/key-value-list';
import { RelativeTime } from '@/components/data/relative-time';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { FormSkeleton } from '@/components/feedback/skeletons';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  Sheet,
  SheetBody,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import type { AppInstantClient, AppLink } from '@/lib/instant';
import { DEFAULT_EXPLORER_PAGE_SIZE } from '@/lib/explorer/url-state';
import { formatCount } from '@/lib/format';
import { useUser } from '@/lib/users/rows';
import {
  userCreatedAt,
  userEmail,
  userLinkGroups,
  userTypeLabel,
  type UserRow,
} from '@/lib/users/users';

const USER_TABS = [
  { id: 'overview', label: 'Overview' },
  { id: 'links', label: 'Linked data' },
  { id: 'json', label: 'JSON' },
] as const;

type UserTab = (typeof USER_TABS)[number]['id'];

function isUserTab(value: unknown): value is UserTab {
  return USER_TABS.some((entry) => entry.id === value);
}

export interface UserSheetProps {
  appId: string;
  client: AppInstantClient;
  userId: string;
  /** Every link that starts at `$users` (C.8). */
  links: ReadonlyArray<AppLink>;
  /** The list row, so the header is populated before the sheet's query lands. */
  fallback: UserRow | null;
  onOpenChange: (open: boolean) => void;
  onSendMagicCode: (user: UserRow) => void;
  onSignOut: (user: UserRow) => void;
  onDelete: (user: UserRow) => void;
  /** A mutation for this user is in flight; the actions stay disabled. */
  busy: boolean;
}

/**
 * C.8 user sheet (`?user=`): overview, linked data fetched on demand, raw JSON
 * and the admin auth actions. Every action needs the app's admin token, so the
 * sheet is only mounted behind the data gate.
 */
export function UserSheet({
  appId,
  client,
  userId,
  links,
  fallback,
  onOpenChange,
  onSendMagicCode,
  onSignOut,
  onDelete,
  busy,
}: UserSheetProps) {
  const [tab, setTab] = useState<UserTab>('overview');
  const linkLabels = useMemo(() => links.map((link) => link.label), [links]);
  const detail = useUser(client, userId, linkLabels);
  const user = detail.user ?? fallback;
  const email = user === null ? null : userEmail(user);
  const groups = userLinkGroups(user, links);

  return (
    <Sheet
      open
      onOpenChange={(next) => {
        if (!next) onOpenChange(false);
      }}
    >
      <SheetContent size="md">
        <SheetHeader>
          <SheetTitle>{email ?? 'Guest user'}</SheetTitle>
          <SheetDescription>
            {email === null
              ? 'This guest has no email address.'
              : 'End user in this app.'}
          </SheetDescription>
          <span className="mt-1 flex flex-wrap items-center gap-2">
            <IdChip value={userId} />
            <CopyButton value={userId} aria-label="Copy user ID" />
          </span>
        </SheetHeader>

        {detail.error !== null ? (
          <SheetBody>
            <ErrorState thing="user" message={detail.error} />
          </SheetBody>
        ) : user === null && detail.isLoading ? (
          <SheetBody>
            <FormSkeleton rows={4} />
          </SheetBody>
        ) : user === null ? (
          <SheetBody>
            <EmptyState
              icon={Users}
              title="User not found"
              description="This user no longer exists in the app."
            />
          </SheetBody>
        ) : (
          <Tabs
            value={tab}
            className="flex min-h-0 flex-1 flex-col"
            onValueChange={(value) => {
              if (isUserTab(value)) setTab(value);
            }}
          >
            <TabsList aria-label="User" className="mx-4">
              {USER_TABS.map((entry) => (
                <TabsTrigger key={entry.id} value={entry.id}>
                  {entry.label}
                </TabsTrigger>
              ))}
            </TabsList>

            <SheetBody className="flex flex-col gap-4">
              <TabsContent value="overview">
                <UserOverview user={user} />
              </TabsContent>

              <TabsContent value="links" className="flex flex-col gap-3">
                {links.length === 0 ? (
                  <p className="text-muted-foreground text-sm">
                    This app has no links from users.
                  </p>
                ) : (
                  groups.map((group) => (
                    <LinkGroup
                      key={group.link.id}
                      appId={appId}
                      label={group.link.label}
                      targetNamespace={group.link.targetNamespace}
                      entities={group.entities}
                    />
                  ))
                )}
              </TabsContent>

              <TabsContent value="json" className="flex flex-col gap-2">
                <UserJson user={user} />
              </TabsContent>
            </SheetBody>
          </Tabs>
        )}

        <SheetFooter className="flex-wrap justify-between">
          <div className="flex min-w-0 flex-col gap-1">
            <div className="flex flex-wrap items-center gap-2">
              {email === null ? null : (
                <Button
                  type="button"
                  variant="secondary"
                  disabled={busy || user === null}
                  onClick={() => {
                    if (user !== null) onSendMagicCode(user);
                  }}
                >
                  Send magic code
                </Button>
              )}
              <Button
                type="button"
                variant="secondary"
                disabled={busy || user === null}
                onClick={() => {
                  if (user !== null) onSignOut(user);
                }}
              >
                Sign out everywhere
              </Button>
            </div>
          </div>
          <Button
            type="button"
            variant="destructive"
            disabled={busy || user === null}
            onClick={() => {
              if (user !== null) onDelete(user);
            }}
          >
            Delete user
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}

function UserOverview({ user }: { user: UserRow }) {
  const email = userEmail(user);
  const createdAt = userCreatedAt(user);
  const items: KeyValueItem[] = [
    {
      label: 'ID',
      value: (
        <span className="flex flex-wrap items-center gap-2">
          <IdChip value={user.id} />
          <CopyButton value={user.id} aria-label="Copy user ID" />
        </span>
      ),
    },
    {
      label: 'Email',
      value:
        email === null ? (
          <Badge tone="neutral">Guest</Badge>
        ) : (
          <span className="text-cell break-all">{email}</span>
        ),
    },
    {
      label: 'Created',
      value:
        createdAt === null ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <RelativeTime value={createdAt} />
        ),
    },
    { label: 'Type', value: userTypeLabel(user) },
  ];

  return <KeyValueList items={items} />;
}

interface LinkGroupProps {
  appId: string;
  label: string;
  targetNamespace: string;
  entities: ReadonlyArray<{ id: string; label: string }>;
}

/** One link from `$users`, with an Explorer jump for every target (C.8). */
function LinkGroup({
  appId,
  label,
  targetNamespace,
  entities,
}: LinkGroupProps) {
  return (
    <div className="border-border flex flex-col gap-2 rounded-md border p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <span className="flex items-center gap-2">
          <span className="text-cell font-mono">{label}</span>
          <Badge tone="accent">{targetNamespace}</Badge>
        </span>
        <span className="text-muted-foreground text-xs">
          {entities.length === 1
            ? '1 entity'
            : `${formatCount(entities.length)} entities`}
        </span>
      </div>

      {entities.length === 0 ? (
        <p className="text-muted-foreground text-sm italic">
          No linked entities
        </p>
      ) : (
        <ul className="flex flex-col gap-1">
          {entities.map((entity) => (
            <li
              key={entity.id}
              className="flex flex-wrap items-center gap-2 text-sm"
            >
              <IdChip value={entity.id} tabIndex={-1} />
              <span className="min-w-0 flex-1 truncate">{entity.label}</span>
              <Link
                to="/apps/$appId/explorer"
                params={{ appId }}
                search={{
                  ns: targetNamespace,
                  where: JSON.stringify({ id: entity.id }),
                  dir: 'asc',
                  size: DEFAULT_EXPLORER_PAGE_SIZE,
                }}
                className="text-accent inline-flex min-h-6 items-center text-xs hover:underline"
              >
                Open in Explorer
              </Link>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** Read-only JSON, with a copy button that carries the whole entity. */
function UserJson({ user }: { user: UserRow }) {
  const payload = JSON.stringify(user, null, 2);
  return (
    <>
      <div className="flex items-center justify-between gap-2">
        <p className="text-muted-foreground text-sm">Read-only user JSON.</p>
        <CopyButton
          value={payload}
          label="Copy JSON"
          aria-label="Copy user JSON"
        />
      </div>
      <CodeEditor
        mode="json"
        aria-label="User JSON"
        value={payload}
        readOnly
        height="360px"
      />
    </>
  );
}
