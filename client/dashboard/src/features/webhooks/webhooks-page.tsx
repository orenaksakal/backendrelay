import { Link, useNavigate } from '@tanstack/react-router';
import { MoreHorizontal, Plus, Webhook as WebhookIcon } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import { CopyButton } from '@/components/data/copy-button';
import { RelativeTime } from '@/components/data/relative-time';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { StatusPill } from '@/components/feedback/status-pill';
import { TableSkeleton } from '@/components/feedback/skeletons';
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
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from '@/components/ui/tooltip';
import { isApiError } from '@/lib/api/client';
import type { Webhook, WebhookEvent } from '@/lib/api/endpoints';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { roleRank } from '@/lib/organization/model';
import { PAGE_ACTION_EVENT } from '@/lib/shortcuts';
import { showErrorToast, toastManager } from '@/lib/toast';
import {
  ACTION_LABELS,
  ACTION_TONES,
  MAX_ACTIVE_WEBHOOKS,
  activeWebhookLimitReached,
  deliveryDisplay,
  namespacesForDisplay,
  normalizeActions,
  webhookLimitNote,
} from '@/lib/webhooks/model';
import {
  useDeleteWebhook,
  useSetWebhookEnabled,
  useWebhookLastDeliveries,
  useWebhookNamespaces,
  useWebhooks,
} from '@/lib/webhooks/queries';
import { DisableWebhookDialog } from './disable-webhook-dialog';
import { WebhookFormSheet } from './webhook-form-sheet';

export interface WebhooksPageProps {
  app: App;
  role: EffectiveRole;
}

/**
 * C.13 list: the URL, watched namespaces, actions, status and last delivery
 * for every webhook, with the create/edit sheet and the disable and delete
 * confirmations.
 */
export function WebhooksPage({ app, role }: WebhooksPageProps) {
  const webhooks = useWebhooks(app.id);
  const namespaces = useWebhookNamespaces(app.id);
  const deleteWebhook = useDeleteWebhook(app.id);
  const setEnabled = useSetWebhookEnabled(app.id);
  const navigate = useNavigate();

  // `undefined` is closed; `null` is the create form.
  const [formWebhook, setFormWebhook] = useState<Webhook | null>();
  const [disabling, setDisabling] = useState<Webhook>();
  const [deleting, setDeleting] = useState<Webhook>();

  const rows = webhooks.data?.webhooks ?? [];
  const lastDeliveries = useWebhookLastDeliveries(
    app.id,
    rows.map((webhook) => webhook.id),
  );
  const canManage = roleRank(role) >= roleRank('collaborator');
  const limitReached = activeWebhookLimitReached(rows);

  const openCreate = useCallback(() => {
    if (!canManage || limitReached) return;
    setFormWebhook(null);
  }, [canManage, limitReached]);

  // N opens the create form (B.7).
  useEffect(() => {
    const listener = (event: Event) => {
      const detail = (event as CustomEvent<{ action?: string } | undefined>)
        .detail;
      if (detail?.action !== 'new') return;
      event.preventDefault();
      openCreate();
    };
    window.addEventListener(PAGE_ACTION_EVENT, listener);
    return () => {
      window.removeEventListener(PAGE_ACTION_EVENT, listener);
    };
  }, [openCreate]);

  function openWebhook(webhook: Webhook): void {
    void navigate({
      to: '/apps/$appId/webhooks/$webhookId',
      params: { appId: app.id, webhookId: webhook.id },
    });
  }

  function enable(webhook: Webhook): void {
    setEnabled.mutate(
      { webhookId: webhook.id, enabled: true },
      {
        onSuccess: () => {
          toastManager.add({ type: 'success', title: 'Webhook enabled' });
        },
      },
    );
  }

  async function confirmDelete(): Promise<void> {
    if (deleting === undefined) return;
    try {
      await deleteWebhook.mutateAsync(deleting.id);
      toastManager.add({ type: 'success', title: 'Webhook deleted' });
      setDeleting(undefined);
    } catch (failure) {
      showErrorToast(failure);
    }
  }

  const columns: ReadonlyArray<SimpleTableColumn<Webhook>> = [
    {
      id: 'url',
      header: 'URL',
      cell: (webhook) => (
        <span className="flex min-w-0 items-center gap-1">
          <Link
            to="/apps/$appId/webhooks/$webhookId"
            params={{ appId: app.id, webhookId: webhook.id }}
            title={webhook.sink.url}
            className="text-accent text-cell block max-w-[22rem] truncate font-mono hover:underline"
          >
            {webhook.sink.url}
          </Link>
          <CopyButton value={webhook.sink.url} aria-label="Copy webhook URL" />
        </span>
      ),
    },
    {
      id: 'namespaces',
      header: 'Namespaces',
      cell: (webhook) => (
        <NamespaceBadges namespaces={webhook.namespaces ?? []} />
      ),
    },
    {
      id: 'actions',
      header: 'Actions',
      cell: (webhook) => (
        <span className="flex flex-wrap gap-1">
          {normalizeActions(webhook.actions).map((action) => (
            <Badge key={action} tone={ACTION_TONES[action]}>
              {ACTION_LABELS[action]}
            </Badge>
          ))}
        </span>
      ),
    },
    {
      id: 'status',
      header: 'Status',
      cell: (webhook) => <WebhookStatus webhook={webhook} />,
    },
    {
      id: 'last-delivery',
      header: 'Last delivery',
      cell: (webhook) => {
        const index = rows.findIndex((entry) => entry.id === webhook.id);
        return (
          <LastDelivery
            event={lastDeliveries[index]?.event}
            loading={lastDeliveries[index]?.isLoading === true}
          />
        );
      },
    },
    {
      id: 'menu',
      header: <span className="sr-only">Actions</span>,
      align: 'right',
      className: 'w-10',
      cell: (webhook) => (
        <WebhookRowMenu
          webhook={webhook}
          canManage={canManage}
          busy={setEnabled.isPending}
          onEdit={() => {
            setFormWebhook(webhook);
          }}
          onToggle={() => {
            if (webhook.status === 'active') {
              setDisabling(webhook);
            } else {
              enable(webhook);
            }
          }}
          onDelete={() => {
            setDeleting(webhook);
          }}
        />
      ),
    },
  ];

  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title="Webhooks"
        description="Receive signed HTTP callbacks when records change in this app."
        actions={
          <Button
            type="button"
            disabled={!canManage || limitReached}
            title={
              canManage
                ? limitReached
                  ? `An app can have ${String(MAX_ACTIVE_WEBHOOKS)} active webhooks.`
                  : undefined
                : 'This needs the collaborator role.'
            }
            onClick={openCreate}
          >
            <Plus aria-hidden="true" />
            Create webhook
          </Button>
        }
      />

      {webhooks.isError && webhooks.data === undefined ? (
        <ErrorState
          thing="webhooks"
          status={
            isApiError(webhooks.error) ? webhooks.error.status : undefined
          }
          message={
            webhooks.error instanceof Error ? webhooks.error.message : undefined
          }
          onRetry={() => {
            void webhooks.refetch();
          }}
        />
      ) : webhooks.isPending ? (
        <TableSkeleton columns={6} />
      ) : rows.length === 0 ? (
        <EmptyState
          icon={WebhookIcon}
          title="No webhooks yet"
          description="Webhooks deliver signed HTTP callbacks to your server when records change."
          action={
            canManage ? (
              <Button type="button" onClick={openCreate}>
                <Plus aria-hidden="true" />
                Create webhook
              </Button>
            ) : undefined
          }
        />
      ) : (
        <>
          <SimpleTable
            caption="Webhooks"
            columns={columns}
            rows={rows}
            getRowId={(webhook) => webhook.id}
            onRowActivate={openWebhook}
          />
          <p className="text-muted-foreground text-xs">
            {webhookLimitNote(rows)}
          </p>
        </>
      )}

      {formWebhook === undefined ? null : (
        <WebhookFormSheet
          key={formWebhook?.id ?? 'new'}
          open
          onOpenChange={(open) => {
            if (!open) setFormWebhook(undefined);
          }}
          appId={app.id}
          webhook={formWebhook ?? undefined}
          schemaNamespaces={namespaces.data ?? []}
          canManage={canManage}
          limitReached={limitReached}
        />
      )}

      {disabling === undefined ? null : (
        <DisableWebhookDialog
          appId={app.id}
          webhook={disabling}
          onOpenChange={(open) => {
            if (!open) setDisabling(undefined);
          }}
        />
      )}

      {deleting === undefined ? null : (
        <ConfirmDialog
          open
          onOpenChange={(open) => {
            if (!open) setDeleting(undefined);
          }}
          title="Delete webhook"
          description={
            <>
              Deleting <span className="font-mono">{deleting.sink.url}</span> is
              permanent. Pending deliveries for it are discarded.
            </>
          }
          confirmLabel="Delete webhook"
          confirming={deleteWebhook.isPending}
          onConfirm={() => {
            void confirmDelete();
          }}
        />
      )}
    </div>
  );
}

function NamespaceBadges({ namespaces }: { namespaces: readonly string[] }) {
  const { shown, extra } = namespacesForDisplay(namespaces);
  if (shown.length === 0) {
    return <span className="text-muted-foreground">—</span>;
  }
  return (
    <span className="flex flex-wrap gap-1">
      {shown.map((namespace) => (
        <Badge key={namespace} tone="neutral" className="font-mono">
          {namespace}
        </Badge>
      ))}
      {extra > 0 ? (
        <Badge tone="neutral" title={namespaces.join(', ')}>
          +{extra}
        </Badge>
      ) : null}
    </span>
  );
}

function WebhookStatus({ webhook }: { webhook: Webhook }) {
  if (webhook.status === 'active') return <StatusPill status="active" />;
  if (
    webhook.disabled_reason === null ||
    webhook.disabled_reason === undefined
  ) {
    return <StatusPill status="disabled" />;
  }
  return (
    <Tooltip>
      <TooltipTrigger render={<span className="inline-flex" />}>
        <StatusPill status="disabled" />
      </TooltipTrigger>
      <TooltipContent>{webhook.disabled_reason}</TooltipContent>
    </Tooltip>
  );
}

function LastDelivery({
  event,
  loading,
}: {
  event: WebhookEvent | undefined;
  loading: boolean;
}) {
  if (event === undefined) {
    return <span className="text-muted-foreground">{loading ? '…' : '—'}</span>;
  }
  const display = deliveryDisplay(event.status);
  return (
    <span className="inline-flex items-center gap-1.5">
      <Badge tone={display.tone}>{display.label}</Badge>
      <RelativeTime
        value={event.created_at}
        className="text-muted-foreground text-xs"
      />
    </span>
  );
}

interface WebhookRowMenuProps {
  webhook: Webhook;
  canManage: boolean;
  busy: boolean;
  onEdit: () => void;
  onToggle: () => void;
  onDelete: () => void;
}

function WebhookRowMenu({
  webhook,
  canManage,
  busy,
  onEdit,
  onToggle,
  onDelete,
}: WebhookRowMenuProps) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        render={
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="size-7 p-0"
            aria-label={`Actions for ${webhook.sink.url}`}
          />
        }
      >
        <MoreHorizontal aria-hidden="true" />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuItem disabled={!canManage} onClick={onEdit}>
          Edit
        </DropdownMenuItem>
        <DropdownMenuItem disabled={!canManage || busy} onClick={onToggle}>
          {webhook.status === 'active' ? 'Disable' : 'Enable'}
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem
          className="text-destructive"
          disabled={!canManage}
          onClick={onDelete}
        >
          Delete
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
