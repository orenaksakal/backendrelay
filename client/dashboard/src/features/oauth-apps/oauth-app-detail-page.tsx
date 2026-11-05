import { Link, useRouter, type NavigateOptions } from '@tanstack/react-router';
import { ArrowLeft, Plus } from 'lucide-react';
import { useId, useRef, useState } from 'react';
import { CopyButton } from '@/components/data/copy-button';
import { IdChip } from '@/components/data/id-chip';
import { RelativeTime } from '@/components/data/relative-time';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { ErrorState } from '@/components/feedback/error-state';
import { CardSkeleton } from '@/components/feedback/skeletons';
import { PageHeader } from '@/components/layout/page-header';
import { SectionCard } from '@/components/layout/section-card';
import { SettingsRow } from '@/components/layout/settings-row';
import { UnsavedChangesBar } from '@/components/layout/unsaved-changes-bar';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { accountTimestamp } from '@/lib/account/dates';
import { isApiError } from '@/lib/api/client';
import type { OauthApp, OauthAppClient } from '@/lib/api/endpoints';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { formatCount } from '@/lib/format';
import { oauthAppErrorMessage } from '@/lib/oauth-apps/errors';
import {
  OAUTH_APP_LOGO_ACCEPT,
  canDeleteOAuthApps,
  canManageOAuthApps,
  canViewOAuthApps,
  clientCount,
  logoProblem,
  oauthAppInfo,
  oauthAppInfoEqual,
  oauthAppInfoPatch,
  oauthAppInfoProblems,
  readFileAsDataUrl,
  type OauthAppInfo,
  type OauthAppInfoField,
} from '@/lib/oauth-apps/model';
import {
  useDeleteOauthApp,
  useDeleteOauthClient,
  useOauthApps,
  useUpdateOauthApp,
} from '@/lib/oauth-apps/queries';
import { toastManager } from '@/lib/toast';
import { OauthAppLogo } from './app-logo';
import { ClientSecretDialog } from './client-secret-dialog';
import { OauthClientSheet } from './oauth-client-sheet';

const SAVE_ERROR = "Couldn't save the OAuth app. Try again.";
const DELETE_ERROR = "Couldn't delete the OAuth app. Try again.";
const DELETE_CLIENT_ERROR = "Couldn't delete the client. Try again.";

export interface OauthAppDetailPageProps {
  app: App;
  role: EffectiveRole;
  oauthAppId: string;
}

/**
 * C.14 OAuth app detail: the app info (saved from the bar), the client table
 * and the one-time client secrets.
 */
export function OauthAppDetailPage({
  app,
  role,
  oauthAppId,
}: OauthAppDetailPageProps) {
  const canView = canViewOAuthApps(role);
  const apps = useOauthApps(app.id, canView);

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

  if (apps.isError) {
    return (
      <ErrorState
        thing="the OAuth app"
        status={isApiError(apps.error) ? apps.error.status : undefined}
        message={apps.error instanceof Error ? apps.error.message : undefined}
        type={isApiError(apps.error) ? apps.error.type : undefined}
        hint={isApiError(apps.error) ? apps.error.hint : undefined}
        requestId={isApiError(apps.error) ? apps.error.requestId : undefined}
        onRetry={() => {
          void apps.refetch();
        }}
      />
    );
  }

  if (apps.isPending) {
    return (
      <div className="flex max-w-4xl flex-col gap-4">
        <CardSkeleton />
        <CardSkeleton />
      </div>
    );
  }

  const oauthApp = apps.data.apps.find(
    (candidate) => candidate.id === oauthAppId,
  );
  if (oauthApp === undefined) {
    return (
      <ErrorState
        thing="that OAuth app"
        status={404}
        message="It may have been deleted. Go back to the list to check."
      />
    );
  }

  return <OauthAppDetail app={app} role={role} oauthApp={oauthApp} />;
}

interface OauthAppDetailProps {
  app: App;
  role: EffectiveRole;
  oauthApp: OauthApp;
}

function OauthAppDetail({ app, role, oauthApp }: OauthAppDetailProps) {
  const router = useRouter();
  const remove = useDeleteOauthApp(app.id);
  const [selectedClientId, setSelectedClientId] = useState<string>('none');
  const [secret, setSecret] = useState<
    { clientName: string; value: string } | undefined
  >();
  const [deleteOpen, setDeleteOpen] = useState(false);

  const selectedClient =
    selectedClientId === 'new'
      ? undefined
      : (oauthApp.clients ?? []).find(
          (client) => client.clientId === selectedClientId,
        );
  const sheetOpen =
    selectedClientId === 'new' ||
    (selectedClientId !== 'none' && selectedClient !== undefined);

  function openListView(): void {
    void router.navigate({
      href: `/apps/${encodeURIComponent(app.id)}/oauth-apps`,
    } as NavigateOptions);
  }

  return (
    <div className="flex max-w-4xl flex-col gap-4">
      <Link
        to="/apps/$appId/oauth-apps"
        params={{ appId: app.id }}
        className="text-accent inline-flex w-fit items-center gap-1 text-sm hover:underline"
      >
        <ArrowLeft className="size-4" aria-hidden="true" />
        OAuth apps
      </Link>

      <PageHeader
        title={oauthApp.appName}
        description="An OAuth provider for the Platform API."
        actions={
          canDeleteOAuthApps(role) ? (
            <Button
              type="button"
              variant="destructive"
              disabled={remove.isPending}
              onClick={() => {
                remove.reset();
                setDeleteOpen(true);
              }}
            >
              Delete OAuth app
            </Button>
          ) : undefined
        }
      />

      {oauthApp.isPublic ? null : (
        <p
          role="note"
          className="border-border bg-muted text-muted-foreground rounded-md border p-3 text-sm"
        >
          This app is in test mode. Only members of {app.title} can sign in with
          it, and redirect URLs may use localhost. Ask the operator to publish
          it before external users sign in.
        </p>
      )}

      <OauthAppInfoCard appId={app.id} role={role} oauthApp={oauthApp} />

      <ClientsCard
        appId={app.id}
        role={role}
        oauthApp={oauthApp}
        onAddClient={() => {
          setSelectedClientId('new');
        }}
        onEditClient={(client) => {
          setSelectedClientId(client.clientId);
        }}
        onClientDeleted={() => {
          setSelectedClientId('none');
        }}
      />

      {remove.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {oauthAppErrorMessage(remove.error, DELETE_ERROR)}
        </p>
      ) : null}

      {sheetOpen ? (
        <OauthClientSheet
          appId={app.id}
          oauthApp={oauthApp}
          client={selectedClient}
          onClose={() => {
            setSelectedClientId('none');
          }}
          onSecret={setSecret}
        />
      ) : null}

      {secret === undefined ? null : (
        <ClientSecretDialog
          clientName={secret.clientName}
          secret={secret.value}
          onClose={() => {
            setSecret(undefined);
          }}
        />
      )}

      <ConfirmDialog
        open={deleteOpen}
        onOpenChange={setDeleteOpen}
        title={`Delete ${oauthApp.appName}?`}
        description="Every client and every token it issued stops working immediately. This cannot be undone."
        confirmLabel="Delete OAuth app"
        confirming={remove.isPending}
        onConfirm={() => {
          remove.mutate(oauthApp.id, {
            onSuccess: () => {
              setDeleteOpen(false);
              toastManager.add({ type: 'success', title: 'OAuth app deleted' });
              openListView();
            },
          });
        }}
      />
    </div>
  );
}

interface OauthAppInfoCardProps {
  appId: string;
  role: EffectiveRole;
  oauthApp: OauthApp;
}

function OauthAppInfoCard({ appId, role, oauthApp }: OauthAppInfoCardProps) {
  const update = useUpdateOauthApp(appId);
  const [draft, setDraft] = useState<OauthAppInfo>();
  const [attempted, setAttempted] = useState(false);
  const [logoError, setLogoError] = useState<string>();
  const logoInput = useRef<HTMLInputElement>(null);

  const nameId = useId();
  const homePageId = useId();
  const privacyId = useId();
  const termsId = useId();
  const emailId = useId();

  const base = oauthAppInfo(oauthApp);
  const info = draft ?? base;
  const dirty = draft !== undefined && !oauthAppInfoEqual(info, base);
  const problems = attempted ? oauthAppInfoProblems(info) : {};
  const editable = canManageOAuthApps(role);

  function updateField(patch: Partial<OauthAppInfo>): void {
    setDraft({ ...info, ...patch });
    update.reset();
  }

  function reset(): void {
    setDraft(undefined);
    setAttempted(false);
    setLogoError(undefined);
    update.reset();
  }

  function save(): void {
    if (update.isPending) return;
    setAttempted(true);
    if (Object.keys(oauthAppInfoProblems(info)).length > 0) return;
    update.mutate(
      {
        oauthAppId: oauthApp.id,
        body: oauthAppInfoPatch(info, base),
      },
      {
        onSuccess: () => {
          setDraft(undefined);
          setAttempted(false);
          toastManager.add({ type: 'success', title: 'OAuth app saved' });
        },
      },
    );
  }

  async function onLogoFile(file: File | undefined): Promise<void> {
    if (file === undefined) return;
    const problem = logoProblem(file);
    if (problem !== undefined) {
      setLogoError(problem);
      return;
    }
    try {
      const dataUrl = await readFileAsDataUrl(file);
      setLogoError(undefined);
      updateField({ logo: dataUrl });
    } catch {
      setLogoError('Could not read that file. Try another one.');
    }
  }

  function errorFor(field: OauthAppInfoField): string | undefined {
    return problems[field];
  }

  const fields: Array<{
    field: OauthAppInfoField;
    id: string;
    label: string;
    description: string;
    placeholder?: string;
    type?: string;
    inputMode?: 'url' | 'email';
  }> = [
    {
      field: 'name',
      id: nameId,
      label: 'Name',
      description: 'Shown on the consent screen.',
    },
    {
      field: 'homePage',
      id: homePageId,
      label: 'Homepage URL',
      description: 'Where users can learn about the integration.',
      placeholder: 'https://example.com',
      inputMode: 'url',
    },
    {
      field: 'privacyPolicy',
      id: privacyId,
      label: 'Privacy policy URL',
      description: 'Must be a public page.',
      placeholder: 'https://example.com/privacy',
      inputMode: 'url',
    },
    {
      field: 'terms',
      id: termsId,
      label: 'Terms of service URL',
      description: 'Must be a public page.',
      placeholder: 'https://example.com/terms',
      inputMode: 'url',
    },
    {
      field: 'supportEmail',
      id: emailId,
      label: 'Support email',
      description: 'Where users ask for help.',
      placeholder: 'support@example.com',
      type: 'email',
      inputMode: 'email',
    },
  ];

  return (
    <div className="flex flex-col gap-4">
      <SectionCard
        title="App info"
        description="What users see when they authorize this integration."
      >
        <div className="flex flex-col gap-4">
          <SettingsRow label="Logo" description="PNG or SVG, up to 256 KB.">
            <div className="flex items-center gap-2">
              <OauthAppLogo
                app={{ appName: info.name, appLogo: info.logo }}
                size="lg"
              />
              <input
                ref={logoInput}
                type="file"
                accept={OAUTH_APP_LOGO_ACCEPT}
                className="hidden"
                aria-label="Upload logo"
                onChange={(event) => {
                  const file = event.target.files?.[0];
                  event.target.value = '';
                  void onLogoFile(file);
                }}
              />
              <Button
                type="button"
                variant="secondary"
                size="sm"
                disabled={!editable || update.isPending}
                onClick={() => {
                  logoInput.current?.click();
                }}
              >
                {info.logo === '' ? 'Upload logo' : 'Replace logo'}
              </Button>
            </div>
          </SettingsRow>
          {logoError === undefined ? null : (
            <p role="alert" className="text-destructive text-xs">
              {logoError}
            </p>
          )}

          {fields.map((entry) => {
            const error = errorFor(entry.field);
            const errorId = `${entry.id}-error`;
            return (
              <SettingsRow
                key={entry.field}
                label={entry.label}
                htmlFor={entry.id}
                description={entry.description}
              >
                <div className="flex w-64 flex-col gap-1">
                  <Input
                    id={entry.id}
                    type={entry.type ?? 'text'}
                    inputMode={entry.inputMode}
                    autoComplete="off"
                    placeholder={entry.placeholder}
                    value={info[entry.field]}
                    disabled={!editable || update.isPending}
                    aria-invalid={error !== undefined}
                    aria-describedby={error === undefined ? undefined : errorId}
                    onChange={(event) => {
                      const patch: Partial<OauthAppInfo> = {};
                      patch[entry.field] = event.target.value;
                      updateField(patch);
                    }}
                  />
                  {error === undefined ? null : (
                    <p
                      id={errorId}
                      role="alert"
                      className="text-destructive text-xs"
                    >
                      {error}
                    </p>
                  )}
                </div>
              </SettingsRow>
            );
          })}
        </div>
      </SectionCard>

      <UnsavedChangesBar
        dirty={dirty}
        saving={update.isPending}
        error={
          update.isError ? oauthAppErrorMessage(update.error, SAVE_ERROR) : null
        }
        onReset={reset}
        onSave={save}
      />
    </div>
  );
}

interface ClientsCardProps {
  appId: string;
  role: EffectiveRole;
  oauthApp: OauthApp;
  onAddClient: () => void;
  onEditClient: (client: OauthAppClient) => void;
  onClientDeleted: () => void;
}

function ClientsCard({
  appId,
  role,
  oauthApp,
  onAddClient,
  onEditClient,
  onClientDeleted,
}: ClientsCardProps) {
  const remove = useDeleteOauthClient(appId);
  const [deleteTarget, setDeleteTarget] = useState<OauthAppClient>();
  const canDelete = canDeleteOAuthApps(role);
  const clients = oauthApp.clients ?? [];

  const columns: SimpleTableColumn<OauthAppClient>[] = [
    {
      id: 'name',
      header: 'Name',
      cell: (client) => (
        <Button
          type="button"
          variant="link"
          size="sm"
          className="h-auto p-0"
          onClick={() => {
            onEditClient(client);
          }}
        >
          {client.clientName}
        </Button>
      ),
    },
    {
      id: 'client-id',
      header: 'Client ID',
      cell: (client) => <IdChip value={client.clientId} />,
    },
    {
      id: 'redirects',
      header: 'Redirect URLs',
      cell: (client) =>
        client.authorizedRedirectUrls.length === 0 ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <ul className="flex flex-col gap-0.5">
            {client.authorizedRedirectUrls.map((url, index) => (
              <li key={url} className="flex items-center gap-1">
                <code
                  title={url}
                  className="text-cell max-w-[260px] truncate font-mono"
                >
                  {url}
                </code>
                <CopyButton
                  value={url}
                  aria-label={`Copy redirect URL ${formatCount(index + 1)}`}
                />
              </li>
            ))}
          </ul>
        ),
    },
    {
      id: 'created',
      header: 'Created',
      cell: (client) => (
        <RelativeTime
          value={accountTimestamp(client.createdAt)}
          className="text-cell"
        />
      ),
    },
  ];

  if (canDelete) {
    columns.push({
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      align: 'right',
      cell: (client) => (
        <Button
          type="button"
          variant="ghost"
          size="sm"
          disabled={remove.isPending}
          onClick={() => {
            remove.reset();
            setDeleteTarget(client);
          }}
        >
          Delete
        </Button>
      ),
    });
  }

  return (
    <div className="flex flex-col gap-4">
      <SectionCard
        title="Clients"
        description={
          clients.length === 0
            ? 'An integration that signs users in to this app.'
            : `${formatCount(clientCount(oauthApp))} ${
                clients.length === 1 ? 'client' : 'clients'
              }.`
        }
        actions={
          <Button type="button" onClick={onAddClient}>
            <Plus aria-hidden="true" />
            Add client
          </Button>
        }
      >
        <SimpleTable
          caption="OAuth clients"
          columns={columns}
          rows={clients}
          getRowId={(client) => client.clientId}
          empty="No clients yet. Add one to let an integration sign users in."
        />
      </SectionCard>

      {remove.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {oauthAppErrorMessage(remove.error, DELETE_CLIENT_ERROR)}
        </p>
      ) : null}

      <ConfirmDialog
        open={deleteTarget !== undefined}
        onOpenChange={(open) => {
          if (open) return;
          setDeleteTarget(undefined);
          remove.reset();
        }}
        title="Delete client?"
        description={
          <>
            Delete{' '}
            <strong className="font-semibold">
              {deleteTarget?.clientName}
            </strong>
            ? Every token it issued stops working immediately.
          </>
        }
        confirmLabel="Delete client"
        confirming={remove.isPending}
        onConfirm={() => {
          if (deleteTarget === undefined) return;
          const clientId = deleteTarget.clientId;
          remove.mutate(
            { oauthAppId: oauthApp.id, clientId },
            {
              onSuccess: () => {
                setDeleteTarget(undefined);
                toastManager.add({
                  type: 'success',
                  title: 'OAuth client deleted',
                });
                onClientDeleted();
              },
            },
          );
        }}
      />
    </div>
  );
}
