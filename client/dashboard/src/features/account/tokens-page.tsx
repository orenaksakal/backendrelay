import { Key, Loader2, Plus } from 'lucide-react';
import { useId, useState } from 'react';
import { CopyButton } from '@/components/data/copy-button';
import { RelativeTime } from '@/components/data/relative-time';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { PageHeader } from '@/components/layout/page-header';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Field, FieldError, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { accountErrorMessage } from '@/lib/account/errors';
import { accountTimestamp } from '@/lib/account/dates';
import {
  useCreatePersonalToken,
  useDeletePersonalToken,
  usePersonalTokens,
} from '@/lib/account/queries';
import { isApiError } from '@/lib/api/client';
import type {
  CreatedPersonalAccessToken,
  PersonalAccessToken,
} from '@/lib/api/endpoints';

const CREATE_ERROR = "Couldn't create the token. Try again.";
const DELETE_ERROR = "Couldn't delete the token. Try again.";

/**
 * C.18 Tokens: personal access tokens for the Platform API. The token value is
 * shown once, in a dialog, because the server stores only a hash.
 */
export function TokensPage() {
  const tokens = usePersonalTokens();
  const createToken = useCreatePersonalToken();
  const deleteToken = useDeletePersonalToken();

  const [createOpen, setCreateOpen] = useState(false);
  const [name, setName] = useState('');
  const [created, setCreated] = useState<
    CreatedPersonalAccessToken | undefined
  >();
  const [deleteTarget, setDeleteTarget] = useState<
    PersonalAccessToken | undefined
  >();
  const [sessionHints, setSessionHints] = useState<Record<string, string>>({});

  const nameId = useId();
  const nameErrorId = `${nameId}-error`;

  const rows = tokens.data?.data ?? [];

  function openCreate(): void {
    createToken.reset();
    setName('');
    setCreateOpen(true);
  }

  function submitCreate(): void {
    const trimmed = name.trim();
    if (trimmed === '' || createToken.isPending) return;
    createToken.mutate(trimmed, {
      onSuccess: (result) => {
        setSessionHints((current) => ({
          ...current,
          [result.data.id]: result.data.token.slice(-4),
        }));
        setCreated(result.data);
        setCreateOpen(false);
        setName('');
      },
    });
  }

  const columns: SimpleTableColumn<PersonalAccessToken>[] = [
    {
      id: 'name',
      header: 'Name',
      cell: (token) => <span className="font-medium">{token.name}</span>,
    },
    {
      id: 'created',
      header: 'Created',
      cell: (token) => (
        <RelativeTime
          value={accountTimestamp(token.created_at)}
          className="text-cell"
        />
      ),
    },
    {
      id: 'last-four',
      header: 'Last 4',
      cell: (token) => (
        <span className="text-cell font-mono">
          {tokenLastFour(token, sessionHints)}
        </span>
      ),
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      align: 'right',
      cell: (token) => (
        <Button
          type="button"
          variant="ghost"
          size="sm"
          onClick={() => {
            deleteToken.reset();
            setDeleteTarget(token);
          }}
        >
          Delete
        </Button>
      ),
    },
  ];

  return (
    <div className="flex max-w-4xl flex-col gap-4">
      <PageHeader
        title="Tokens"
        description="Credentials for the Platform API. Treat them like passwords."
        actions={
          <Button type="button" onClick={openCreate}>
            <Plus aria-hidden="true" />
            Create token
          </Button>
        }
      />

      {tokens.isError ? (
        <ErrorState
          thing="personal access tokens"
          status={isApiError(tokens.error) ? tokens.error.status : undefined}
          message={
            tokens.error instanceof Error ? tokens.error.message : undefined
          }
          requestId={
            isApiError(tokens.error) ? tokens.error.requestId : undefined
          }
          onRetry={() => {
            void tokens.refetch();
          }}
        />
      ) : rows.length === 0 && !tokens.isPending ? (
        <EmptyState
          icon={Key}
          title="No personal access tokens"
          description="Create a token to call the Platform API from scripts or CI."
          action={
            <Button type="button" onClick={openCreate}>
              <Plus aria-hidden="true" />
              Create token
            </Button>
          }
        />
      ) : (
        <SimpleTable
          caption="Personal access tokens"
          columns={columns}
          rows={rows}
          getRowId={(token) => token.id}
          loading={tokens.isPending}
          empty="No personal access tokens"
        />
      )}

      {deleteToken.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {accountErrorMessage(deleteToken.error, DELETE_ERROR)}
        </p>
      ) : null}

      <Dialog
        open={createOpen}
        onOpenChange={(open) => {
          setCreateOpen(open);
          if (!open) {
            createToken.reset();
            setName('');
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Create personal access token</DialogTitle>
            <DialogDescription>
              Give the token a name so you can tell it apart later.
            </DialogDescription>
          </DialogHeader>
          <form
            className="mt-4 flex flex-col gap-4"
            onSubmit={(event) => {
              event.preventDefault();
              submitCreate();
            }}
          >
            <Field invalid={createToken.isError}>
              <FieldLabel htmlFor={nameId}>Name</FieldLabel>
              <Input
                id={nameId}
                value={name}
                placeholder="CI deploy"
                aria-invalid={createToken.isError || undefined}
                aria-describedby={createToken.isError ? nameErrorId : undefined}
                onChange={(event) => {
                  setName(event.target.value);
                  createToken.reset();
                }}
              />
              {createToken.isError ? (
                <FieldError id={nameErrorId} role="alert">
                  {accountErrorMessage(createToken.error, CREATE_ERROR)}
                </FieldError>
              ) : null}
            </Field>
            <DialogFooter className="mt-0">
              <Button
                type="button"
                variant="secondary"
                onClick={() => {
                  setCreateOpen(false);
                  createToken.reset();
                  setName('');
                }}
              >
                Cancel
              </Button>
              <Button
                type="submit"
                disabled={createToken.isPending || name.trim() === ''}
                aria-busy={createToken.isPending}
              >
                {createToken.isPending ? (
                  <Loader2 className="animate-spin" aria-hidden="true" />
                ) : null}
                Create token
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      <Dialog
        open={created !== undefined}
        onOpenChange={() => {
          // The token is shown once: only "I've saved it" closes the dialog.
        }}
      >
        <DialogContent showCloseButton={false}>
          <DialogHeader>
            <DialogTitle>Copy your token</DialogTitle>
            <DialogDescription>
              This is the only time the token is shown. Store it somewhere safe;
              you will have to create a new one if it is lost.
            </DialogDescription>
          </DialogHeader>
          <div className="border-input bg-muted mt-4 flex items-start gap-2 rounded-md border p-3">
            <code className="text-cell min-w-0 flex-1 font-mono break-all">
              {created?.token}
            </code>
            <CopyButton value={created?.token ?? ''} aria-label="Copy token" />
          </div>
          <DialogFooter>
            <Button
              type="button"
              onClick={() => {
                setCreated(undefined);
                createToken.reset();
              }}
            >
              I&apos;ve saved it
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={deleteTarget !== undefined}
        onOpenChange={(open) => {
          if (!open) {
            setDeleteTarget(undefined);
            deleteToken.reset();
          }
        }}
        title="Delete personal access token?"
        description={
          <>
            Delete{' '}
            <strong className="font-semibold">{deleteTarget?.name}</strong>?
            Anything using it will lose access immediately.
          </>
        }
        confirmLabel="Delete token"
        confirming={deleteToken.isPending}
        onConfirm={() => {
          if (deleteTarget === undefined) return;
          deleteToken.mutate(deleteTarget.id, {
            onSuccess: () => {
              setDeleteTarget(undefined);
            },
          });
        }}
      />
    </div>
  );
}

function tokenLastFour(
  token: PersonalAccessToken,
  hints: Record<string, string>,
): string {
  return (
    token.last_four ??
    token.lastFour ??
    token.token_last_four ??
    hints[token.id] ??
    '—'
  );
}
