import { AppWindow } from 'lucide-react';
import { useState } from 'react';
import { RelativeTime } from '@/components/data/relative-time';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { PageHeader } from '@/components/layout/page-header';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { accountErrorMessage } from '@/lib/account/errors';
import { accountTimestamp } from '@/lib/account/dates';
import {
  useAuthorizedApps,
  useRevokeAuthorizedApp,
} from '@/lib/account/queries';
import { isApiError } from '@/lib/api/client';
import type { AuthorizedOauthApp } from '@/lib/api/endpoints';

const REVOKE_ERROR = "Couldn't revoke access. Try again.";

/**
 * C.18 OAuth apps: third-party applications the operator authorized. Revoking
 * deletes the user's tokens for that app, so the operator must confirm it.
 */
export function OAuthAppsPage() {
  const apps = useAuthorizedApps();
  const revoke = useRevokeAuthorizedApp();
  const [revokeTarget, setRevokeTarget] = useState<
    AuthorizedOauthApp | undefined
  >();

  const rows = apps.data?.oauthApps ?? [];

  const columns: SimpleTableColumn<AuthorizedOauthApp>[] = [
    {
      id: 'name',
      header: 'App',
      cell: (app) => <AppCell app={app} />,
    },
    {
      id: 'scopes',
      header: 'Scopes',
      cell: (app) => {
        const scopes = app.scopes ?? app.grantedScopes ?? app.granted_scopes;
        return scopes !== undefined && scopes.length > 0 ? (
          <span className="flex flex-wrap gap-1">
            {scopes.map((scope) => (
              <Badge key={scope} tone="neutral">
                {scope}
              </Badge>
            ))}
          </span>
        ) : (
          <span className="text-muted-foreground">—</span>
        );
      },
    },
    {
      id: 'authorized',
      header: 'Authorized',
      cell: (app) => {
        const authorizedAt =
          app.authorizedAt ??
          app.authorized_at ??
          app.createdAt ??
          app.created_at;
        return authorizedAt === undefined ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <RelativeTime
            value={accountTimestamp(authorizedAt)}
            className="text-cell"
          />
        );
      },
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      align: 'right',
      cell: (app) => (
        <Button
          type="button"
          variant="ghost"
          size="sm"
          onClick={() => {
            revoke.reset();
            setRevokeTarget(app);
          }}
        >
          Revoke access
        </Button>
      ),
    },
  ];

  return (
    <div className="flex max-w-4xl flex-col gap-4">
      <PageHeader
        title="OAuth apps"
        description="Applications you authorized to act on your account."
      />

      {apps.isError ? (
        <ErrorState
          thing="authorized apps"
          status={isApiError(apps.error) ? apps.error.status : undefined}
          message={apps.error instanceof Error ? apps.error.message : undefined}
          requestId={isApiError(apps.error) ? apps.error.requestId : undefined}
          onRetry={() => {
            void apps.refetch();
          }}
        />
      ) : rows.length === 0 && !apps.isPending ? (
        <EmptyState
          icon={AppWindow}
          title="No authorized apps"
          description="When an OAuth app asks for access, it appears here."
        />
      ) : (
        <SimpleTable
          caption="Authorized OAuth apps"
          columns={columns}
          rows={rows}
          getRowId={(app) => app.id}
          loading={apps.isPending}
          empty="No authorized apps"
        />
      )}

      {revoke.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {accountErrorMessage(revoke.error, REVOKE_ERROR)}
        </p>
      ) : null}

      <ConfirmDialog
        open={revokeTarget !== undefined}
        onOpenChange={(open) => {
          if (!open) {
            setRevokeTarget(undefined);
            revoke.reset();
          }
        }}
        title="Revoke access?"
        description={
          <>
            Revoke access for{' '}
            <strong className="font-semibold">{revokeTarget?.name}</strong>? It
            will need your permission the next time it connects.
          </>
        }
        confirmLabel="Revoke access"
        confirming={revoke.isPending}
        onConfirm={() => {
          if (revokeTarget === undefined) return;
          revoke.mutate(revokeTarget.id, {
            onSuccess: () => {
              setRevokeTarget(undefined);
            },
          });
        }}
      />
    </div>
  );
}

function AppCell({ app }: { app: AuthorizedOauthApp }) {
  const homePage = safeHttpUrl(app.homePage);

  return (
    <div className="flex flex-col gap-0.5">
      {homePage === undefined ? (
        <span className="font-medium">{app.name}</span>
      ) : (
        <a
          href={homePage}
          target="_blank"
          rel="noopener noreferrer"
          className="text-accent font-medium hover:underline"
        >
          {app.name}
        </a>
      )}
      <PolicyLinks app={app} />
    </div>
  );
}

function PolicyLinks({ app }: { app: AuthorizedOauthApp }) {
  const tos = safeHttpUrl(app.tosLink);
  const privacy = safeHttpUrl(app.privacyPolicyLink);

  if (tos === undefined && privacy === undefined) return null;

  return (
    <span className="text-muted-foreground flex gap-3 text-xs">
      {tos === undefined ? null : (
        <a
          href={tos}
          target="_blank"
          rel="noopener noreferrer"
          className="hover:text-foreground inline-flex min-h-6 items-center"
        >
          Terms
        </a>
      )}
      {privacy === undefined ? null : (
        <a
          href={privacy}
          target="_blank"
          rel="noopener noreferrer"
          className="hover:text-foreground inline-flex min-h-6 items-center"
        >
          Privacy
        </a>
      )}
    </span>
  );
}

/** Only absolute http(s) links without credentials are rendered (C.18). */
function safeHttpUrl(value: string | null | undefined): string | undefined {
  if (value === null || value === undefined || value === '') return undefined;
  try {
    const url = new URL(value);
    if (url.protocol !== 'https:' && url.protocol !== 'http:') return undefined;
    if (url.username !== '' || url.password !== '') return undefined;
    return url.href;
  } catch {
    return undefined;
  }
}
