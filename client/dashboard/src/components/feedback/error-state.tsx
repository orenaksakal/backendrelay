import { AlertTriangle } from 'lucide-react';
import { CopyButton } from '@/components/data/copy-button';
import {
  KeyValueList,
  type KeyValueItem,
} from '@/components/data/key-value-list';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';

export interface ErrorStateProps {
  /** What failed to load, for example "users" or "app". */
  thing: string;
  /** HTTP status, which picks the 403 and 404 wording. */
  status?: number;
  /** Server message shown under the title. */
  message?: string;
  type?: string;
  hint?: unknown;
  requestId?: string;
  /** 403 only: the role that would grant access. */
  requiredRole?: string;
  /** Apps get their own 404 wording (B.4). */
  kind?: 'app' | 'default';
  onRetry?: () => void;
  className?: string;
}

function errorTitle(
  thing: string,
  status: number | undefined,
  kind: 'app' | 'default',
): string {
  if (status === 403) {
    return `You don't have access to ${thing}`;
  }
  if (status === 404 && kind === 'app') {
    return "App not found, or you don't have access.";
  }
  if (status === 404) {
    return `Couldn't find ${thing}`;
  }
  return `Couldn't load ${thing}`;
}

/**
 * Query failure state: title, server message, request ID, Retry and a
 * "Show details" disclosure with the status, error type and hint (B.4).
 */
export function ErrorState({
  thing,
  status,
  message,
  type,
  hint,
  requestId,
  requiredRole,
  kind = 'default',
  onRetry,
  className,
}: ErrorStateProps) {
  const detailItems: KeyValueItem[] = [];
  if (status !== undefined) {
    detailItems.push({ label: 'Status', value: status });
  }
  if (type) {
    detailItems.push({ label: 'Error type', value: type });
  }
  if (hint !== undefined) {
    detailItems.push({
      label: 'Hint',
      value: (
        <code className="font-mono text-xs break-all">
          {JSON.stringify(hint, null, 2)}
        </code>
      ),
    });
  }

  return (
    <div
      role="alert"
      className={cn(
        'border-border bg-card flex max-w-[560px] flex-col gap-2 rounded-lg border p-4',
        className,
      )}
    >
      <div className="flex items-start gap-2">
        <AlertTriangle
          className="text-destructive mt-0.5 size-4 shrink-0"
          aria-hidden="true"
        />
        <div className="flex min-w-0 flex-col gap-1">
          <p className="text-section">{errorTitle(thing, status, kind)}</p>
          {message ? (
            <p className="text-muted-foreground text-sm">{message}</p>
          ) : null}
          {status === 403 && requiredRole ? (
            <p className="text-muted-foreground text-sm">
              This needs the {requiredRole} role.
            </p>
          ) : null}
          {requestId ? (
            <p className="text-muted-foreground flex items-center gap-1 text-xs">
              Request ID: <span className="font-mono text-xs">{requestId}</span>
              <CopyButton value={requestId} aria-label="Copy request ID" />
            </p>
          ) : null}
        </div>
      </div>

      <div className="flex flex-wrap items-center gap-2">
        {onRetry ? (
          <Button type="button" variant="secondary" size="sm" onClick={onRetry}>
            Retry
          </Button>
        ) : null}
        {detailItems.length > 0 ? (
          <details className="text-muted-foreground text-xs">
            <summary className="hover:text-foreground inline-flex min-h-6 cursor-pointer items-center">
              Show details
            </summary>
            <KeyValueList className="mt-2" items={detailItems} />
          </details>
        ) : null}
      </div>
    </div>
  );
}
