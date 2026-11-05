import { useMemo } from 'react';
import { CopyButton } from '@/components/data/copy-button';
import { RelativeTime } from '@/components/data/relative-time';
import { KeyValueList } from '@/components/data/key-value-list';
import { CodeBlock } from '@/components/code-editor/code-block';
import { ErrorState } from '@/components/feedback/error-state';
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
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from '@/components/ui/tooltip';
import type { WebhookAttempt, WebhookEvent } from '@/lib/api/endpoints';
import { isApiError } from '@/lib/api/client';
import { toastManager } from '@/lib/toast';
import {
  attemptAt,
  attemptDuration,
  attemptErrorMessage,
  attemptErrorType,
  attemptResponseText,
  attemptStatusCode,
  attemptSucceeded,
  attemptsNewestFirst,
  canResendEvent,
  deliveryAttemptCount,
  deliveryDisplay,
  isEventInFlight,
} from '@/lib/webhooks/model';
import {
  useResendWebhookEvent,
  useWebhookEvent,
  useWebhookPayload,
} from '@/lib/webhooks/queries';

export interface DeliverySheetProps {
  appId: string;
  webhookId: string;
  isn: string;
  onClose: () => void;
}

/**
 * C.13 delivery sheet (880px, `?event=`): the payload snapshot, every attempt
 * with its response, and Resend inside the 30-day window.
 */
export function DeliverySheet({
  appId,
  webhookId,
  isn,
  onClose,
}: DeliverySheetProps) {
  const eventQuery = useWebhookEvent(appId, webhookId, isn);
  const payload = useWebhookPayload(appId, webhookId, isn);
  const resend = useResendWebhookEvent(appId, webhookId);

  const event = eventQuery.data?.event;
  const tooOld = event === undefined ? true : !canResendEvent(event);

  async function resendEvent(): Promise<void> {
    try {
      await resend.mutateAsync(isn);
      toastManager.add({ type: 'success', title: 'Event resent' });
    } catch {
      // The global mutation toast carries the server message.
    }
  }

  return (
    <Sheet
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <SheetContent size="lg">
        <SheetHeader>
          <SheetTitle>Delivery</SheetTitle>
          <SheetDescription className="font-mono text-xs">
            {isn}
          </SheetDescription>
        </SheetHeader>
        <SheetBody>
          {eventQuery.isError && event === undefined ? (
            <ErrorState
              thing="delivery"
              status={
                isApiError(eventQuery.error)
                  ? eventQuery.error.status
                  : undefined
              }
              message={
                eventQuery.error instanceof Error
                  ? eventQuery.error.message
                  : undefined
              }
              onRetry={() => {
                void eventQuery.refetch();
              }}
            />
          ) : event === undefined ? (
            <p className="text-muted-foreground text-sm">Loading delivery…</p>
          ) : (
            <div className="flex flex-col gap-6">
              <DeliveryDetails event={event} />
              <AttemptsSection event={event} />
              <PayloadSection
                isLoading={payload.isPending}
                unavailable={payload.data?.data === null}
                failed={payload.isError}
                value={payload.data}
              />
            </div>
          )}
        </SheetBody>
        <SheetFooter>
          <Button type="button" variant="secondary" onClick={onClose}>
            Close
          </Button>
          {event === undefined ? (
            <Button type="button" disabled>
              Resend
            </Button>
          ) : tooOld ? (
            <Tooltip>
              <TooltipTrigger render={<span className="inline-flex" />}>
                <Button type="button" disabled>
                  Resend
                </Button>
              </TooltipTrigger>
              <TooltipContent>
                Events older than 30 days can&apos;t be resent
              </TooltipContent>
            </Tooltip>
          ) : (
            <Button
              type="button"
              disabled={resend.isPending}
              onClick={() => {
                void resendEvent();
              }}
            >
              {resend.isPending ? 'Resending…' : 'Resend'}
            </Button>
          )}
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}

function DeliveryDetails({ event }: { event: WebhookEvent }) {
  const display = deliveryDisplay(event.status);
  return (
    <section className="flex flex-col gap-2">
      <h3 className="text-section">Details</h3>
      <KeyValueList
        items={[
          {
            label: 'Status',
            value: (
              <span className="inline-flex items-center gap-2">
                <Badge tone={display.tone}>{display.label}</Badge>
                {isEventInFlight(event) ? (
                  <span className="text-muted-foreground text-xs">
                    Retrying automatically
                  </span>
                ) : null}
              </span>
            ),
          },
          {
            label: 'Event ID',
            value: (
              <span className="inline-flex items-center gap-1">
                <span className="font-mono text-xs break-all">{event.isn}</span>
                <CopyButton value={event.isn} aria-label="Copy event ID" />
              </span>
            ),
          },
          {
            label: 'Created',
            value: <RelativeTime value={event.created_at} />,
          },
          {
            label: 'Attempts',
            value: deliveryAttemptCount(event),
          },
          {
            label: 'Next attempt',
            value:
              event.next_attempt_after === null ||
              event.next_attempt_after === undefined ? (
                '—'
              ) : (
                <RelativeTime value={event.next_attempt_after} />
              ),
          },
        ]}
      />
    </section>
  );
}

function AttemptsSection({ event }: { event: WebhookEvent }) {
  const attempts = useMemo(() => attemptsNewestFirst(event), [event]);
  return (
    <section className="flex flex-col gap-2">
      <h3 className="text-section">Responses</h3>
      {attempts.length === 0 ? (
        <p className="text-muted-foreground text-sm">No requests yet.</p>
      ) : (
        <ol className="flex flex-col gap-3">
          {attempts.map((attempt, index) => (
            <AttemptRow
              key={`${attempt['attempt-at'] ?? 'pending'}:${String(index)}`}
              attempt={attempt}
              number={attempts.length - index}
            />
          ))}
        </ol>
      )}
    </section>
  );
}

function AttemptRow({
  attempt,
  number,
}: {
  attempt: WebhookAttempt;
  number: number;
}) {
  const code = attemptStatusCode(attempt);
  const duration = attemptDuration(attempt);
  const success = attemptSucceeded(attempt) === true;
  const errorMessage = attemptErrorMessage(attempt);
  const responseText = attemptResponseText(attempt);
  const sentAt = attemptAt(attempt);

  return (
    <li className="border-border flex flex-col gap-1.5 rounded-md border p-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-sm font-medium">Attempt {number}</span>
        {sentAt ? (
          <RelativeTime
            value={sentAt}
            className="text-muted-foreground text-xs"
          />
        ) : (
          <span className="text-muted-foreground text-xs">Not sent yet</span>
        )}
        <Badge tone={success ? 'success' : 'destructive'}>
          {success ? 'Delivered' : 'Failed'}
        </Badge>
        <span className="text-muted-foreground font-mono text-xs">
          {code === undefined ? 'No response' : `HTTP ${String(code)}`}
          {duration === undefined ? '' : ` · ${String(duration)} ms`}
        </span>
      </div>
      {attemptErrorType(attempt) ? (
        <p className="text-muted-foreground text-xs">
          {attemptErrorType(attempt)}
        </p>
      ) : null}
      {errorMessage ? <CodeBlock code={errorMessage} language="error" /> : null}
      {responseText ? (
        <CodeBlock code={responseText} language="response" />
      ) : null}
    </li>
  );
}

function PayloadSection({
  isLoading,
  unavailable,
  failed,
  value,
}: {
  isLoading: boolean;
  unavailable: boolean;
  failed: boolean;
  value: { data: unknown[] | null; idempotencyKey: string } | undefined;
}) {
  return (
    <section className="flex flex-col gap-2">
      <h3 className="text-section">Payload</h3>
      {isLoading ? (
        <p className="text-muted-foreground text-sm">Loading payload…</p>
      ) : failed || value === undefined || unavailable ? (
        <p className="text-muted-foreground text-sm">
          The payload snapshot is no longer available.
        </p>
      ) : (
        <CodeBlock
          language="json"
          code={JSON.stringify(
            { idempotencyKey: value.idempotencyKey, data: value.data },
            null,
            2,
          )}
        />
      )}
    </section>
  );
}
