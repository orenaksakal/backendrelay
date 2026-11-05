import { useNavigate } from '@tanstack/react-router';
import { ChevronDown, RefreshCw } from 'lucide-react';
import { useState } from 'react';
import { CodeBlock } from '@/components/code-editor/code-block';
import { CopyButton } from '@/components/data/copy-button';
import { RelativeTime } from '@/components/data/relative-time';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { ErrorState } from '@/components/feedback/error-state';
import { StatusPill } from '@/components/feedback/status-pill';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { PageHeader } from '@/components/layout/page-header';
import { SectionCard } from '@/components/layout/section-card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field';
import { isApiError } from '@/lib/api/client';
import type { Webhook, WebhookEvent } from '@/lib/api/endpoints';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { roleRank } from '@/lib/organization/model';
import { getRuntimeConfig } from '@/lib/runtime-config';
import { showErrorToast, toastManager } from '@/lib/toast';
import { cn } from '@/lib/utils';
import {
  anyEventInFlight,
  deliveryAttemptCount,
  deliveryDisplay,
  deliveryDuration,
  deliveryHttpCode,
  jwksUrl,
  verificationSnippet,
} from '@/lib/webhooks/model';
import {
  useDeleteWebhook,
  useSetWebhookEnabled,
  useWebhookEvents,
  useWebhookNamespaces,
  useWebhooks,
} from '@/lib/webhooks/queries';
import { DeliverySheet } from './delivery-sheet';
import { DisableWebhookDialog } from './disable-webhook-dialog';
import { WebhookFormSheet } from './webhook-form-sheet';

export interface WebhookDetailPageProps {
  app: App;
  role: EffectiveRole;
  webhookId: string;
  /** `?event=`: the delivery sheet is open on this ISN. */
  selectedEvent: string | undefined;
  onSelectedEventChange: (isn: string | undefined) => void;
}

/**
 * C.13 detail: the webhook header, the Signing card with the JWKS URL and the
 * verification snippet, and the paginated deliveries table.
 */
export function WebhookDetailPage({
  app,
  role,
  webhookId,
  selectedEvent,
  onSelectedEventChange,
}: WebhookDetailPageProps) {
  const webhooks = useWebhooks(app.id);
  const namespaces = useWebhookNamespaces(app.id);
  const deleteWebhook = useDeleteWebhook(app.id);
  const setEnabled = useSetWebhookEnabled(app.id);
  const navigate = useNavigate();

  const [editing, setEditing] = useState(false);
  const [disabling, setDisabling] = useState<Webhook>();
  const [deleting, setDeleting] = useState<Webhook>();

  const webhook = webhooks.data?.webhooks.find(
    (candidate) => candidate.id === webhookId,
  );
  const canManage = roleRank(role) >= roleRank('collaborator');

  if (webhooks.isError && webhooks.data === undefined) {
    return (
      <ErrorState
        thing="webhook"
        status={isApiError(webhooks.error) ? webhooks.error.status : undefined}
        message={
          webhooks.error instanceof Error ? webhooks.error.message : undefined
        }
        onRetry={() => {
          void webhooks.refetch();
        }}
      />
    );
  }

  if (webhooks.isPending) {
    return <TableSkeleton columns={5} />;
  }

  if (webhook === undefined) {
    return <ErrorState thing="webhook" status={404} />;
  }

  function enable(): void {
    setEnabled.mutate(
      { webhookId, enabled: true },
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
      void navigate({
        to: '/apps/$appId/webhooks',
        params: { appId: app.id },
      });
    } catch (failure) {
      showErrorToast(failure);
    }
  }

  const apiBaseUrl = getRuntimeConfig()?.apiBaseUrl ?? '';
  const signingUrl = jwksUrl(apiBaseUrl);

  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title={webhook.sink.url}
        className="[&_h1]:break-all"
        description={
          <span className="flex flex-wrap items-center gap-2">
            <StatusPill
              status={webhook.status === 'active' ? 'active' : 'disabled'}
            />
            {webhook.disabled_reason ? (
              <span className="text-xs">{webhook.disabled_reason}</span>
            ) : null}
            <span className="inline-flex items-center gap-1 text-xs">
              Created <RelativeTime value={webhook.created_at} />
            </span>
          </span>
        }
        actions={
          <>
            <Button
              type="button"
              variant="secondary"
              disabled={!canManage}
              onClick={() => {
                setEditing(true);
              }}
            >
              Edit
            </Button>
            {webhook.status === 'active' ? (
              <Button
                type="button"
                variant="secondary"
                disabled={!canManage}
                onClick={() => {
                  setDisabling(webhook);
                }}
              >
                Disable
              </Button>
            ) : (
              <Button
                type="button"
                variant="secondary"
                disabled={!canManage || setEnabled.isPending}
                onClick={enable}
              >
                Enable
              </Button>
            )}
            <Button
              type="button"
              variant="destructive"
              disabled={!canManage}
              onClick={() => {
                setDeleting(webhook);
              }}
            >
              Delete
            </Button>
          </>
        }
      />

      <SigningCard appId={app.id} apiBaseUrl={apiBaseUrl} jwks={signingUrl} />

      <DeliveriesSection
        appId={app.id}
        webhookId={webhook.id}
        onOpenEvent={(isn) => {
          onSelectedEventChange(isn);
        }}
      />

      {editing ? (
        <WebhookFormSheet
          key={webhook.id}
          open
          onOpenChange={(open) => {
            if (!open) setEditing(false);
          }}
          appId={app.id}
          webhook={webhook}
          schemaNamespaces={namespaces.data ?? []}
          canManage={canManage}
          limitReached={false}
        />
      ) : null}

      <DisableWebhookDialog
        appId={app.id}
        webhook={disabling}
        onOpenChange={(open) => {
          if (!open) setDisabling(undefined);
        }}
      />

      <ConfirmDialog
        open={deleting !== undefined}
        onOpenChange={(open) => {
          if (!open) setDeleting(undefined);
        }}
        title="Delete webhook"
        description={
          deleting === undefined ? (
            ''
          ) : (
            <>
              Deleting <span className="font-mono">{deleting.sink.url}</span> is
              permanent. Pending deliveries for it are discarded.
            </>
          )
        }
        confirmLabel="Delete webhook"
        confirming={deleteWebhook.isPending}
        onConfirm={() => {
          void confirmDelete();
        }}
      />

      {selectedEvent === undefined ? null : (
        <DeliverySheet
          key={selectedEvent}
          appId={app.id}
          webhookId={webhook.id}
          isn={selectedEvent}
          onClose={() => {
            onSelectedEventChange(undefined);
          }}
        />
      )}
    </div>
  );
}

function SigningCard({
  appId,
  apiBaseUrl,
  jwks,
}: {
  appId: string;
  apiBaseUrl: string;
  jwks: string;
}) {
  return (
    <SectionCard
      title="Signing"
      description="Verify the Instant-Signature header before acting on a delivery."
    >
      <div className="flex flex-col gap-4">
        <Field>
          <FieldLabel>JWKS URL</FieldLabel>
          <div className="border-input bg-muted flex min-h-8 items-center gap-1 rounded-md border px-2 py-1">
            <code className="min-w-0 flex-1 truncate font-mono text-xs">
              {jwks}
            </code>
            <CopyButton value={jwks} aria-label="Copy JWKS URL" />
          </div>
          <FieldDescription>
            Keys rotate; fetch this URL instead of pinning a public key.
          </FieldDescription>
        </Field>
        <CodeBlock
          language="ts"
          code={verificationSnippet({ appId, apiBaseUrl })}
        />
      </div>
    </SectionCard>
  );
}

function DeliveriesSection({
  appId,
  webhookId,
  onOpenEvent,
}: {
  appId: string;
  webhookId: string;
  onOpenEvent: (isn: string) => void;
}) {
  const events = useWebhookEvents(appId, webhookId);
  const rows = events.data?.pages.flatMap((page) => page.events) ?? [];
  const inFlight = anyEventInFlight(rows);

  const columns: ReadonlyArray<SimpleTableColumn<WebhookEvent>> = [
    {
      id: 'time',
      header: 'Time',
      cell: (event) => (
        <RelativeTime value={event.created_at} className="text-cell" />
      ),
    },
    {
      id: 'event',
      header: 'Event',
      cell: (event) => (
        <Button
          type="button"
          variant="link"
          size="sm"
          className="max-w-[18rem] justify-start truncate font-mono text-xs"
          title={event.isn}
          onClick={() => {
            onOpenEvent(event.isn);
          }}
        >
          {event.isn}
        </Button>
      ),
    },
    {
      id: 'status',
      header: 'Status',
      cell: (event) => <DeliveryStatus event={event} />,
    },
    {
      id: 'attempts',
      header: 'Attempts',
      align: 'right',
      cell: (event) => deliveryAttemptCount(event),
    },
    {
      id: 'duration',
      header: 'Duration',
      align: 'right',
      cell: (event) => {
        const duration = deliveryDuration(event);
        return duration === undefined ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          `${String(duration)} ms`
        );
      },
    },
  ];

  return (
    <SectionCard
      title="Deliveries"
      description="Every delivery attempt for this webhook, newest first."
      actions={
        <Button
          type="button"
          variant="ghost"
          size="sm"
          aria-label="Refresh deliveries"
          disabled={events.isFetching}
          onClick={() => {
            void events.refetch();
          }}
        >
          <RefreshCw
            aria-hidden="true"
            className={cn(events.isFetching && 'animate-spin')}
          />
          Refresh
        </Button>
      }
    >
      <div className="flex flex-col gap-2">
        {events.isError && events.data === undefined ? (
          <ErrorState
            thing="deliveries"
            status={isApiError(events.error) ? events.error.status : undefined}
            message={
              events.error instanceof Error ? events.error.message : undefined
            }
            onRetry={() => {
              void events.refetch();
            }}
          />
        ) : events.isPending ? (
          <TableSkeleton columns={5} />
        ) : (
          <>
            <SimpleTable
              caption="Webhook deliveries"
              columns={columns}
              rows={rows}
              getRowId={(event) => event.isn}
              onRowActivate={(event) => {
                onOpenEvent(event.isn);
              }}
              empty={
                <span className="text-muted-foreground">
                  No deliveries yet. New events show up here within a few
                  seconds.
                </span>
              }
            />
            <div className="flex flex-wrap items-center justify-between gap-2">
              <p className="text-muted-foreground text-xs" role="status">
                {inFlight
                  ? 'Refreshing every 5 seconds while a delivery is pending.'
                  : `${String(rows.length)} ${rows.length === 1 ? 'delivery' : 'deliveries'} loaded.`}
              </p>
              {events.hasNextPage ? (
                <Button
                  type="button"
                  variant="secondary"
                  size="sm"
                  disabled={events.isFetchingNextPage}
                  onClick={() => {
                    void events.fetchNextPage();
                  }}
                >
                  <ChevronDown aria-hidden="true" />
                  {events.isFetchingNextPage ? 'Loading…' : 'Load more'}
                </Button>
              ) : null}
            </div>
          </>
        )}
      </div>
    </SectionCard>
  );
}

function DeliveryStatus({ event }: { event: WebhookEvent }) {
  const display = deliveryDisplay(event.status);
  const code = deliveryHttpCode(event);
  return (
    <span className="inline-flex flex-wrap items-center gap-1.5">
      <Badge tone={display.tone}>{display.label}</Badge>
      {code === undefined ? null : (
        <span className="text-muted-foreground font-mono text-xs">
          HTTP {code}
        </span>
      )}
    </span>
  );
}
