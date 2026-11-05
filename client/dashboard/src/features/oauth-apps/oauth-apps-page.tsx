import { Link, useRouter, type NavigateOptions } from '@tanstack/react-router';
import { AppWindow, Plus } from 'lucide-react';
import { useState } from 'react';
import { RelativeTime } from '@/components/data/relative-time';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { PageHeader } from '@/components/layout/page-header';
import { Button } from '@/components/ui/button';
import { accountTimestamp } from '@/lib/account/dates';
import { isApiError } from '@/lib/api/client';
import type { OauthApp } from '@/lib/api/endpoints';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { formatCount } from '@/lib/format';
import {
  canManageOAuthApps,
  canViewOAuthApps,
  clientCount,
} from '@/lib/oauth-apps/model';
import { useOauthApps } from '@/lib/oauth-apps/queries';
import { CreateOauthAppSheet } from './create-oauth-app-sheet';
import { OauthAppLogo } from './app-logo';

export interface OauthAppsPageProps {
  app: App;
  role: EffectiveRole;
}

/**
 * C.14 OAuth apps list: the OAuth providers published for this app. Most apps
 * never use it, so the empty state explains it in one sentence.
 */
export function OauthAppsPage({ app, role }: OauthAppsPageProps) {
  const router = useRouter();
  const canView = canViewOAuthApps(role);
  const apps = useOauthApps(app.id, canView);
  const [createOpen, setCreateOpen] = useState(false);

  if (!canView) {
    return (
      <ErrorState
        thing="OAuth apps"
        status={403}
        requiredRole="collaborator"
        message="Only app collaborators, administrators and owners can manage OAuth apps."
      />
    );
  }

  const rows = apps.data?.apps ?? [];

  const columns: SimpleTableColumn<OauthApp>[] = [
    {
      id: 'app',
      header: 'App',
      cell: (row) => (
        <span className="flex items-center gap-3">
          <OauthAppLogo app={row} />
          <Link
            to="/apps/$appId/oauth-apps/$oauthAppId"
            params={{ appId: app.id, oauthAppId: row.id }}
            className="font-medium hover:underline"
          >
            {row.appName}
          </Link>
        </span>
      ),
    },
    {
      id: 'clients',
      header: 'Clients',
      cell: (row) => (
        <span className="tabular-nums">{formatCount(clientCount(row))}</span>
      ),
    },
    {
      id: 'created',
      header: 'Created',
      cell: (row) => (
        <RelativeTime
          value={accountTimestamp(row.createdAt)}
          className="text-cell"
        />
      ),
    },
  ];

  function openCreated(created: OauthApp): void {
    setCreateOpen(false);
    void router.navigate({
      href: `/apps/${encodeURIComponent(app.id)}/oauth-apps/${encodeURIComponent(created.id)}`,
    } as NavigateOptions);
  }

  return (
    <div className="flex max-w-4xl flex-col gap-4">
      <PageHeader
        title="OAuth apps"
        description="Let other tools sign users in and call the Platform API on their behalf."
        actions={
          canManageOAuthApps(role) ? (
            <Button
              type="button"
              onClick={() => {
                setCreateOpen(true);
              }}
            >
              <Plus aria-hidden="true" />
              Create OAuth app
            </Button>
          ) : undefined
        }
      />

      {apps.isError ? (
        <ErrorState
          thing="OAuth apps"
          status={isApiError(apps.error) ? apps.error.status : undefined}
          message={apps.error instanceof Error ? apps.error.message : undefined}
          type={isApiError(apps.error) ? apps.error.type : undefined}
          hint={isApiError(apps.error) ? apps.error.hint : undefined}
          requestId={isApiError(apps.error) ? apps.error.requestId : undefined}
          onRetry={() => {
            void apps.refetch();
          }}
        />
      ) : apps.isPending ? (
        <TableSkeleton columns={3} />
      ) : rows.length === 0 ? (
        <EmptyState
          icon={AppWindow}
          title="No OAuth apps"
          description="Publish this app as an OAuth provider only if you are building an integration that needs one."
          action={
            canManageOAuthApps(role) ? (
              <Button
                type="button"
                onClick={() => {
                  setCreateOpen(true);
                }}
              >
                <Plus aria-hidden="true" />
                Create OAuth app
              </Button>
            ) : undefined
          }
        />
      ) : (
        <SimpleTable
          caption="OAuth apps"
          columns={columns}
          rows={rows}
          getRowId={(row) => row.id}
          loading={apps.isPending}
          empty="No OAuth apps"
        />
      )}

      {createOpen ? (
        <CreateOauthAppSheet
          appId={app.id}
          onClose={() => {
            setCreateOpen(false);
          }}
          onCreated={openCreated}
        />
      ) : null}
    </div>
  );
}
