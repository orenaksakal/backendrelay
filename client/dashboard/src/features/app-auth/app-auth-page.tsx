import { Plus } from 'lucide-react';
import { useState } from 'react';
import { IdChip } from '@/components/data/id-chip';
import { RelativeTime } from '@/components/data/relative-time';
import { SimpleTable } from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { PageHeader } from '@/components/layout/page-header';
import { SectionCard } from '@/components/layout/section-card';
import { Button } from '@/components/ui/button';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { isApiError } from '@/lib/api/client';
import type { OAuthClient } from '@/lib/api/endpoints';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import {
  AUTH_PROVIDERS,
  AUTH_PROVIDER_DEFINITIONS,
  MAGIC_CODE_PROVIDER,
  clientsForProvider,
  providerLabel,
  providerStatus,
  providersById,
  providersByName,
  type AuthProvider,
  type AuthProviderDefinition,
} from '@/lib/app-auth/providers';
import { useAppAuth, useDeleteClient } from '@/lib/app-auth/queries';
import {
  APP_AUTH_TABS,
  APP_AUTH_TAB_LABELS,
  addProviderSearch,
  clientSheetSearch,
  closedSheetSearch,
  isAppAuthTab,
  type AppAuthSearch,
} from '@/lib/app-auth/url-state';
import { roleRank } from '@/lib/organization/model';
import { toastManager } from '@/lib/toast';
import { EmailTab } from './email-tab';
import { OriginsTab } from './origins-tab';
import { ProviderSheet } from './provider-sheet';
import { TestUsersTab } from './test-users-tab';

export interface AppAuthPageProps {
  app: App;
  role: EffectiveRole;
  search: AppAuthSearch;
  onSearchChange: (search: AppAuthSearch) => void;
}

interface ProviderCardProps {
  definition: AuthProviderDefinition;
  clientCount: number;
  canManage: boolean;
  onConfigure: () => void;
}

function ProviderCard({
  definition,
  clientCount,
  canManage,
  onConfigure,
}: ProviderCardProps) {
  const Icon = definition.icon;
  return (
    <div className="border-border bg-card flex items-center gap-3 rounded-lg border p-4">
      <div className="bg-muted text-muted-foreground flex size-10 shrink-0 items-center justify-center rounded-md">
        <Icon aria-hidden="true" className="size-5" />
      </div>
      <div className="flex min-w-0 flex-1 flex-col gap-0.5">
        <p className="text-section">{definition.label}</p>
        <p className="text-muted-foreground text-xs">
          {providerStatus(clientCount)}
        </p>
        <p className="text-muted-foreground truncate text-xs">
          {definition.description}
        </p>
      </div>
      <Button
        type="button"
        variant="secondary"
        size="sm"
        disabled={!canManage}
        aria-label={`Configure ${definition.label}`}
        title={canManage ? undefined : 'This needs the collaborator role.'}
        onClick={onConfigure}
      >
        Configure
      </Button>
    </div>
  );
}

interface ClientsTableProps {
  appId: string;
  clients: ReadonlyArray<OAuthClient>;
  providerNames: Record<string, string>;
  canManage: boolean;
  onEdit: (client: OAuthClient) => void;
}

function ClientsTable({
  appId,
  clients,
  providerNames,
  canManage,
  onEdit,
}: ClientsTableProps) {
  const deleteClient = useDeleteClient(appId);
  const [deleting, setDeleting] = useState<OAuthClient>();

  async function confirmDelete(): Promise<void> {
    if (deleting === undefined) return;
    try {
      await deleteClient.mutateAsync(deleting.id);
      toastManager.add({ type: 'success', title: 'Client deleted' });
      setDeleting(undefined);
    } catch {
      // The global mutation toast carries the server message.
    }
  }

  return (
    <SectionCard
      title="Clients"
      description="Every OAuth client users can sign in with."
    >
      <SimpleTable
        caption="OAuth clients"
        columns={[
          {
            id: 'provider',
            header: 'Provider',
            cell: (client) =>
              providerLabel(providerNames[client.provider_id] ?? ''),
          },
          {
            id: 'name',
            header: 'Name',
            cell: (client) => (
              <span className="font-medium">{client.client_name}</span>
            ),
          },
          {
            id: 'client-id',
            header: 'Client ID',
            cell: (client) =>
              client.client_id ? (
                <IdChip value={client.client_id} />
              ) : (
                <span className="text-muted-foreground">—</span>
              ),
          },
          {
            id: 'created',
            header: 'Created',
            cell: (client) =>
              client.created_at === undefined ? (
                <span className="text-muted-foreground">—</span>
              ) : (
                <RelativeTime value={client.created_at} />
              ),
          },
          {
            id: 'actions',
            header: <span className="sr-only">Actions</span>,
            align: 'right',
            cell: (client) => (
              <div className="flex items-center justify-end gap-1">
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  aria-label={`Edit ${client.client_name}`}
                  onClick={() => {
                    onEdit(client);
                  }}
                >
                  Edit
                </Button>
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  className="text-destructive"
                  disabled={!canManage}
                  aria-label={`Delete ${client.client_name}`}
                  onClick={() => {
                    setDeleting(client);
                  }}
                >
                  Delete
                </Button>
              </div>
            ),
          },
        ]}
        rows={clients}
        getRowId={(client) => client.id}
        onRowActivate={onEdit}
        empty={
          <EmptyState
            icon={Plus}
            title="No OAuth clients"
            description="Configure a provider to let users sign in with it."
          />
        }
      />
      <ConfirmDialog
        open={deleting !== undefined}
        onOpenChange={(next) => {
          if (!next) setDeleting(undefined);
        }}
        title="Delete client"
        description={
          deleting === undefined
            ? ''
            : `Deleting ${deleting.client_name} stops users from signing in with it. Remove references to it in your app first.`
        }
        confirmLabel="Delete client"
        confirming={deleteClient.isPending}
        onConfirm={() => {
          void confirmDelete();
        }}
      />
    </SectionCard>
  );
}

/**
 * C.11 authentication: the provider, redirect-origin, email and test-user tabs,
 * plus the provider sheet for `?add=`/`?client=`.
 */
export function AppAuthPage({
  app,
  role,
  search,
  onSearchChange,
}: AppAuthPageProps) {
  const auth = useAppAuth(app.id);
  const canManage = roleRank(role) >= roleRank('collaborator');
  const clients = auth.data?.oauth_clients ?? [];
  const providers = auth.data?.oauth_service_providers ?? [];
  const origins = auth.data?.authorized_redirect_origins ?? [];
  const byName = providersByName(providers);
  const byId = providersById(providers);

  const loading = auth.isPending;
  const loadError =
    auth.isError && auth.data === undefined ? (
      <ErrorState
        thing="authentication settings"
        status={isApiError(auth.error) ? auth.error.status : undefined}
        message={auth.error instanceof Error ? auth.error.message : undefined}
        onRetry={() => {
          void auth.refetch();
        }}
      />
    ) : undefined;

  const selectedClient =
    search.client === undefined
      ? undefined
      : clients.find((client) => client.id === search.client);
  const selectedProvider = selectedClient
    ? byId[selectedClient.provider_id]
    : undefined;
  const addProvider =
    search.add === undefined
      ? undefined
      : AUTH_PROVIDERS.find((provider) => provider === search.add);
  const sheetProviderName = selectedProvider?.provider_name ?? addProvider;
  const sheetProvider =
    sheetProviderName === undefined
      ? undefined
      : AUTH_PROVIDERS.find((provider) => provider === sheetProviderName);
  const sheetDefinition =
    sheetProvider === undefined
      ? undefined
      : AUTH_PROVIDER_DEFINITIONS[sheetProvider];

  function configureProvider(provider: AuthProvider): void {
    const record = byName[provider];
    const firstClient = clientsForProvider(clients, record?.id)[0];
    onSearchChange(
      firstClient === undefined
        ? addProviderSearch(search, provider)
        : clientSheetSearch(search, firstClient.id),
    );
  }

  return (
    <>
      <Tabs
        value={search.tab}
        className="flex flex-col gap-4"
        onValueChange={(value) => {
          if (isAppAuthTab(value)) {
            onSearchChange({ tab: value });
          }
        }}
      >
        <PageHeader
          title="Auth"
          description="Sign-in providers, redirect origins, email and test users."
          tabs={
            <TabsList aria-label="Authentication settings">
              {APP_AUTH_TABS.map((tab) => (
                <TabsTrigger key={tab} value={tab}>
                  {APP_AUTH_TAB_LABELS[tab]}
                </TabsTrigger>
              ))}
            </TabsList>
          }
        />

        <TabsContent value="providers">
          {loading ? (
            <TableSkeleton rows={4} columns={3} />
          ) : (
            (loadError ?? (
              <div className="flex flex-col gap-4">
                <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
                  {AUTH_PROVIDERS.map((provider) => {
                    const definition = AUTH_PROVIDER_DEFINITIONS[provider];
                    const count = clientsForProvider(
                      clients,
                      byName[provider]?.id,
                    ).length;
                    return (
                      <ProviderCard
                        key={provider}
                        definition={definition}
                        clientCount={count}
                        canManage={canManage}
                        onConfigure={() => {
                          configureProvider(provider);
                        }}
                      />
                    );
                  })}
                  <div className="border-border bg-card flex items-center gap-3 rounded-lg border p-4">
                    <div className="bg-muted text-muted-foreground flex size-10 shrink-0 items-center justify-center rounded-md">
                      <MAGIC_CODE_PROVIDER.icon
                        aria-hidden="true"
                        className="size-5"
                      />
                    </div>
                    <div className="flex min-w-0 flex-1 flex-col gap-0.5">
                      <p className="text-section">
                        {MAGIC_CODE_PROVIDER.label}
                      </p>
                      <p className="text-muted-foreground text-xs">
                        {MAGIC_CODE_PROVIDER.description}
                      </p>
                    </div>
                  </div>
                </div>
                <ClientsTable
                  appId={app.id}
                  clients={clients}
                  providerNames={Object.fromEntries(
                    Object.entries(byId).map(
                      ([id, provider]) => [id, provider.provider_name] as const,
                    ),
                  )}
                  canManage={canManage}
                  onEdit={(client) => {
                    onSearchChange(clientSheetSearch(search, client.id));
                  }}
                />
              </div>
            ))
          )}
        </TabsContent>

        <TabsContent value="origins">
          {loading ? (
            <TableSkeleton rows={4} columns={4} />
          ) : (
            (loadError ?? (
              <OriginsTab
                appId={app.id}
                origins={origins}
                canManage={canManage}
              />
            ))
          )}
        </TabsContent>

        <TabsContent value="email">
          {loading ? (
            <TableSkeleton rows={4} columns={2} />
          ) : (
            (loadError ?? (
              <EmailTab
                appId={app.id}
                appTitle={app.title}
                role={role}
                defaultSenderEmail={auth.data?.default_sender_email ?? ''}
              />
            ))
          )}
        </TabsContent>

        <TabsContent value="test-users">
          <TestUsersTab
            appId={app.id}
            canManage={roleRank(role) >= roleRank('admin')}
          />
        </TabsContent>
      </Tabs>

      {sheetDefinition === undefined ? null : (
        <ProviderSheet
          key={`${sheetDefinition.id}:${selectedClient?.id ?? 'new'}`}
          open
          onOpenChange={(next) => {
            if (!next) onSearchChange(closedSheetSearch(search));
          }}
          appId={app.id}
          provider={sheetDefinition}
          existingProvider={byName[sheetDefinition.id]}
          client={selectedClient}
          clients={clients}
          canManage={canManage}
          onAddAnother={() => {
            onSearchChange(addProviderSearch(search, sheetDefinition.id));
          }}
        />
      )}
    </>
  );
}
