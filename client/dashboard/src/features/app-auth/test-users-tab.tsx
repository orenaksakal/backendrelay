import { KeyRound, Plus } from 'lucide-react';
import { useState } from 'react';
import { CopyButton } from '@/components/data/copy-button';
import { RelativeTime } from '@/components/data/relative-time';
import { SimpleTable } from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { SectionCard } from '@/components/layout/section-card';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { isApiError } from '@/lib/api/client';
import type { TestUser } from '@/lib/api/endpoints';
import {
  testUsersFrom,
  useAddTestUser,
  useDeleteTestUser,
  useTestUsers,
} from '@/lib/app-auth/queries';
import { toastManager } from '@/lib/toast';

const EMAIL_PATTERN = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
const CODE_PATTERN = /^\d{6}$/;

export interface TestUsersTabProps {
  appId: string;
  canManage: boolean;
}

interface AddTestUserDialogProps {
  appId: string;
  onOpenChange: (open: boolean) => void;
}

function AddTestUserDialog({ appId, onOpenChange }: AddTestUserDialogProps) {
  const addTestUser = useAddTestUser(appId);
  const [email, setEmail] = useState('');
  const [code, setCode] = useState('424242');
  const [error, setError] = useState<string>();

  async function add(): Promise<void> {
    const normalizedEmail = email.trim().toLowerCase();
    if (!EMAIL_PATTERN.test(normalizedEmail)) {
      setError('Enter a valid email address.');
      return;
    }
    if (!CODE_PATTERN.test(code)) {
      setError('The code must be exactly 6 digits.');
      return;
    }
    setError(undefined);
    try {
      const result = await addTestUser.mutateAsync({
        email: normalizedEmail,
        code,
      });
      toastManager.add({
        type: 'success',
        title: `${result['test-user'].email} can sign in with ${
          result['test-user'].code
        }`,
      });
      onOpenChange(false);
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "Couldn't add the test user.",
      );
    }
  }

  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Add test user</DialogTitle>
          <DialogDescription>
            This address signs in with the code below instead of receiving an
            email. It never expires.
          </DialogDescription>
        </DialogHeader>
        <div className="mt-4 flex flex-col gap-4">
          <Field>
            <FieldLabel htmlFor="test-user-email">Email</FieldLabel>
            <Input
              id="test-user-email"
              type="email"
              autoComplete="off"
              value={email}
              placeholder="test@example.com"
              aria-invalid={error !== undefined}
              onChange={(event) => {
                setEmail(event.target.value);
                setError(undefined);
              }}
            />
          </Field>
          <Field>
            <FieldLabel htmlFor="test-user-code">Fixed code</FieldLabel>
            <Input
              id="test-user-code"
              inputMode="numeric"
              autoComplete="off"
              maxLength={6}
              value={code}
              placeholder="123456"
              aria-invalid={error !== undefined}
              onChange={(event) => {
                setCode(event.target.value.replace(/\D/g, '').slice(0, 6));
                setError(undefined);
              }}
            />
            <FieldDescription>Exactly 6 digits.</FieldDescription>
          </Field>
          {error === undefined ? null : (
            <p role="alert" className="text-destructive text-sm">
              {error}
            </p>
          )}
        </div>
        <DialogFooter>
          <Button
            type="button"
            variant="secondary"
            onClick={() => {
              onOpenChange(false);
            }}
          >
            Cancel
          </Button>
          <Button
            type="button"
            disabled={addTestUser.isPending}
            onClick={() => {
              void add();
            }}
          >
            {addTestUser.isPending ? 'Adding…' : 'Add test user'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/**
 * C.11 test-users tab: fixed codes that skip email delivery, for development,
 * automated tests and app-store review. Adding needs the admin role.
 */
export function TestUsersTab({ appId, canManage }: TestUsersTabProps) {
  const testUsers = useTestUsers(appId);
  const deleteTestUser = useDeleteTestUser(appId);
  const [addOpen, setAddOpen] = useState(false);
  const [deleting, setDeleting] = useState<TestUser>();
  const [revealed, setRevealed] = useState<ReadonlyArray<string>>([]);

  const users = testUsersFrom(testUsers.data);

  function toggleRevealed(id: string): void {
    setRevealed((current) =>
      current.includes(id)
        ? current.filter((value) => value !== id)
        : [...current, id],
    );
  }

  async function confirmDelete(): Promise<void> {
    if (deleting === undefined) return;
    try {
      await deleteTestUser.mutateAsync(deleting.id);
      toastManager.add({ type: 'success', title: 'Test user removed' });
      setDeleting(undefined);
    } catch {
      // The global mutation toast carries the server message.
    }
  }

  if (testUsers.isError && testUsers.data === undefined) {
    return (
      <ErrorState
        thing="test users"
        status={
          isApiError(testUsers.error) ? testUsers.error.status : undefined
        }
        message={
          testUsers.error instanceof Error ? testUsers.error.message : undefined
        }
        onRetry={() => {
          void testUsers.refetch();
        }}
      />
    );
  }

  return (
    <SectionCard
      title="Test users"
      description="Fixed sign-in codes that skip email delivery, for development, automated testing and app-store review."
      actions={
        <Button
          type="button"
          size="sm"
          disabled={!canManage}
          title={
            canManage
              ? undefined
              : 'Only app administrators and owners can add test users.'
          }
          onClick={() => {
            setAddOpen(true);
          }}
        >
          <Plus aria-hidden="true" />
          Add test user
        </Button>
      }
    >
      <SimpleTable
        caption="Test users"
        loading={testUsers.isPending}
        columns={[
          {
            id: 'email',
            header: 'Email',
            cell: (user) => <span className="font-medium">{user.email}</span>,
          },
          {
            id: 'code',
            header: 'Fixed code',
            cell: (user) => {
              const visible = revealed.includes(user.id);
              return (
                <div className="flex items-center gap-1">
                  <code className="bg-muted text-cell rounded px-1.5 py-0.5 font-mono">
                    {visible ? user.code : '••••••'}
                  </code>
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    aria-label={`${visible ? 'Hide' : 'Reveal'} code for ${user.email}`}
                    onClick={() => {
                      toggleRevealed(user.id);
                    }}
                  >
                    {visible ? 'Hide' : 'Reveal'}
                  </Button>
                  <CopyButton
                    value={user.code}
                    aria-label={`Copy code for ${user.email}`}
                  />
                </div>
              );
            },
          },
          {
            id: 'created',
            header: 'Created',
            cell: (user) => <RelativeTime value={user.created_at} />,
          },
          {
            id: 'actions',
            header: <span className="sr-only">Actions</span>,
            align: 'right',
            cell: (user) => (
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="text-destructive"
                disabled={!canManage}
                aria-label={`Delete test user ${user.email}`}
                onClick={() => {
                  setDeleting(user);
                }}
              >
                Delete
              </Button>
            ),
          },
        ]}
        rows={users}
        getRowId={(user) => user.id}
        empty={
          <EmptyState
            icon={KeyRound}
            title="No test users"
            description="Add a fixed code so a test account can sign in without email."
            action={
              <Button
                type="button"
                size="sm"
                disabled={!canManage}
                title={
                  canManage
                    ? undefined
                    : 'Only app administrators and owners can add test users.'
                }
                onClick={() => {
                  setAddOpen(true);
                }}
              >
                Add test user
              </Button>
            }
          />
        }
      />

      {addOpen ? (
        <AddTestUserDialog appId={appId} onOpenChange={setAddOpen} />
      ) : null}

      <ConfirmDialog
        open={deleting !== undefined}
        onOpenChange={(next) => {
          if (!next) setDeleting(undefined);
        }}
        title="Remove test user"
        description={
          deleting === undefined
            ? ''
            : `Remove the fixed code for ${deleting.email}? They will need a real emailed code to sign in.`
        }
        confirmLabel="Remove test user"
        confirming={deleteTestUser.isPending}
        onConfirm={() => {
          void confirmDelete();
        }}
      />
    </SectionCard>
  );
}
