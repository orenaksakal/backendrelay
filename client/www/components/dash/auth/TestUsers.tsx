import { useContext, useState, type ReactNode } from 'react';
import config from '@/lib/config';
import { jsonMutate } from '@/lib/fetch';
import { TokenContext } from '@/lib/contexts';
import { useAuthedFetch } from '@/lib/auth';
import { InstantApp } from '@/lib/types';
import {
  Button,
  BlockHeading,
  Content,
  Dialog,
  SectionHeading,
  TextInput,
  useDialog,
} from '@/components/ui';
import { errorToast, successToast } from '@/lib/toast';
import { TrashIcon } from '@heroicons/react/24/solid';

export type TestUser = {
  id: string;
  app_id: string;
  email: string;
  code: string;
  created_at: string;
};
export function TestUsersWorkspace({
  users,
  loading,
  error,
  onRefresh,
  onRemove,
  form,
}: {
  users: TestUser[];
  loading: boolean;
  error: boolean;
  onRefresh: () => void;
  onRemove: (user: TestUser) => void;
  form: ReactNode;
}) {
  const [revealed, setRevealed] = useState<Set<string>>(new Set());
  return (
    <div className="flex flex-col gap-2">
      <BlockHeading className="text-sm font-semibold text-gray-700 dark:text-neutral-300">
        Test users
      </BlockHeading>
      <p className="text-sm text-gray-500 dark:text-neutral-400">
        Static magic codes that never expire, so a test user can sign in with a
        fixed code instead of a real email. Handy for development, automated
        testing, and app store review.
      </p>
      {error ? (
        <div className="text-sm text-red-600" role="alert">
          Could not load test users. Refresh to try again.
          <Button size="mini" variant="subtle" onClick={onRefresh}>
            Refresh users
          </Button>
        </div>
      ) : loading ? (
        <div className="text-sm text-gray-400" role="status">
          Loading test users…
        </div>
      ) : users.length > 0 ? (
        <div className="divide-y overflow-hidden rounded-sm border bg-gray-50 dark:divide-neutral-700 dark:border-neutral-700 dark:bg-neutral-800/50">
          {users.map((u) => (
            <div
              key={u.id}
              className="flex items-center justify-between gap-3 px-4 py-3"
            >
              <span className="truncate font-medium">{u.email}</span>
              <div className="flex shrink-0 items-center gap-3">
                <span className="text-xs text-gray-400 dark:text-neutral-500">
                  signs in with
                </span>
                <code className="rounded-sm bg-gray-100 px-2 py-0.5 font-mono text-sm dark:bg-neutral-700">
                  {revealed.has(u.id) ? u.code : '••••••'}
                </code>
                <button
                  className="text-xs text-gray-500 hover:underline dark:text-neutral-400"
                  aria-label={`${revealed.has(u.id) ? 'Hide' : 'Reveal'} code for ${u.email}`}
                  onClick={() =>
                    setRevealed((prev) => {
                      const next = new Set(prev);
                      if (next.has(u.id)) next.delete(u.id);
                      else next.add(u.id);
                      return next;
                    })
                  }
                >
                  {revealed.has(u.id) ? 'Hide' : 'Reveal'}
                </button>
                <button
                  className="text-xs text-gray-500 hover:underline dark:text-neutral-400"
                  aria-label={`Copy code for ${u.email}`}
                  onClick={async () => {
                    try {
                      await navigator.clipboard.writeText(u.code);
                      successToast('Code copied.');
                    } catch {
                      errorToast('Could not copy code.');
                    }
                  }}
                >
                  Copy
                </button>
                <button
                  className="cursor-pointer text-gray-400 hover:text-red-500 dark:text-neutral-500 dark:hover:text-red-400"
                  aria-label={`Remove test user ${u.email}`}
                  onClick={() => onRemove(u)}
                >
                  <TrashIcon height="1rem" />
                </button>
              </div>
            </div>
          ))}
          <div className="px-4 py-3">{form}</div>
        </div>
      ) : (
        form
      )}
    </div>
  );
}
export function TestUsers({ app }: { app: InstantApp }) {
  const token = useContext(TokenContext);
  const [email, setEmail] = useState('');
  const [code, setCode] = useState('424242');
  const [isAdding, setIsAdding] = useState(false);
  const [deletingUser, setDeletingUser] = useState<TestUser | null>(null);
  const [isDeleting, setIsDeleting] = useState(false);
  const [formError, setFormError] = useState('');
  const [deleteError, setDeleteError] = useState('');
  const deleteDialog = useDialog();
  const fetchResult = useAuthedFetch<{ 'test-users': TestUser[] }>(
    `${config.apiURI}/dash/apps/${app.id}/test_users`,
  );
  const users = fetchResult.data?.['test-users'] || [];
  async function add() {
    const normalizedEmail = email.trim().toLowerCase();
    setFormError('');
    if (!normalizedEmail) {
      setFormError('Enter an email address.');
      return;
    }
    if (!/^\d{6}$/.test(code)) {
      setFormError('The code must contain exactly 6 digits.');
      return;
    }
    setIsAdding(true);
    try {
      await jsonMutate(`${config.apiURI}/dash/apps/${app.id}/test_users`, {
        body: { email: normalizedEmail, code },
        token,
      });
      setEmail('');
      setCode('424242');
      successToast('Test user added.');
      fetchResult.mutate();
    } catch (e: any) {
      setFormError(
        e?.body?.type === 'record-not-unique'
          ? 'A test user with this email already exists.'
          : e?.body?.message || 'Failed to add test user.',
      );
    } finally {
      setIsAdding(false);
    }
  }
  async function remove() {
    if (!deletingUser) return;
    setIsDeleting(true);
    setDeleteError('');
    try {
      await jsonMutate(`${config.apiURI}/dash/apps/${app.id}/test_users`, {
        body: { id: deletingUser.id },
        token,
        method: 'DELETE',
      });
      successToast('Test user removed.');
      deleteDialog.onClose();
      fetchResult.mutate();
    } catch {
      setDeleteError('Could not remove test user. Try again.');
    } finally {
      setIsDeleting(false);
    }
  }
  return (
    <>
      <TestUsersWorkspace
        users={users}
        loading={fetchResult.isLoading}
        error={!!fetchResult.error}
        onRefresh={() => fetchResult.mutate()}
        onRemove={(u) => {
          setDeletingUser(u);
          setDeleteError('');
          deleteDialog.onOpen();
        }}
        form={
          <form
            className="flex flex-wrap items-center gap-3"
            onSubmit={(e) => {
              e.preventDefault();
              add();
            }}
          >
            <div className="grow">
              <TextInput
                aria-label="Test user email"
                type="email"
                placeholder="test@example.com"
                value={email}
                onChange={setEmail}
                required
              />
            </div>
            <span className="shrink-0 text-xs text-gray-400 dark:text-neutral-500">
              signs in with
            </span>
            <div className="w-28 shrink-0">
              <TextInput
                aria-label="Fixed sign-in code"
                placeholder="123456"
                value={code}
                onChange={setCode}
                inputMode="numeric"
              />
            </div>
            <Button
              type="submit"
              loading={isAdding}
              disabled={fetchResult.isLoading || !!fetchResult.error}
              variant="secondary"
            >
              Add
            </Button>
            {formError && (
              <div className="w-full text-sm text-red-600" role="alert">
                {formError}
              </div>
            )}
          </form>
        }
      />
      <Dialog title="Remove test user" {...deleteDialog}>
        <div className="flex flex-col gap-4">
          <SectionHeading>Remove test user</SectionHeading>
          <Content>
            Remove the fixed sign-in code for{' '}
            <strong>{deletingUser?.email}</strong>?
          </Content>
          {deleteError && (
            <div className="text-sm text-red-600" role="alert">
              {deleteError}
            </div>
          )}
          <div className="flex justify-end gap-2">
            <Button
              variant="secondary"
              onClick={deleteDialog.onClose}
              disabled={isDeleting}
            >
              Cancel
            </Button>
            <Button variant="destructive" loading={isDeleting} onClick={remove}>
              Remove test user
            </Button>
          </div>
        </div>
      </Dialog>
    </>
  );
}
