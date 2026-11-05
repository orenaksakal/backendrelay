import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { AppInstantClient } from '@/lib/instant';
import { NewNamespaceDialog } from './new-namespace-dialog';

type PushOps = (ops: ReadonlyArray<ReadonlyArray<unknown>>) => Promise<unknown>;

function fakeClient(
  pushOps: PushOps = () => Promise.resolve(undefined),
): AppInstantClient {
  return {
    core: {
      shutdown: () => undefined,
      subscribeQuery: () => () => undefined,
      _reactor: {
        subscribeAttrs: () => () => undefined,
        pushOps,
      },
    },
    tx: {},
    transact: () => Promise.resolve(undefined),
  };
}

function renderDialog() {
  const onCreated = vi.fn();
  const onOpenChange = vi.fn();
  const pushOps = vi.fn<PushOps>(() => Promise.resolve(undefined));
  const client = fakeClient(pushOps);
  render(
    <NewNamespaceDialog
      open
      onOpenChange={onOpenChange}
      client={client}
      existing={['todos']}
      onCreated={onCreated}
    />,
  );
  return { onCreated, onOpenChange, pushOps };
}

describe('new namespace dialog', () => {
  it('rejects names the plan does not allow', async () => {
    const user = userEvent.setup();
    const { pushOps } = renderDialog();

    await user.type(
      screen.getByRole('textbox', { name: 'Namespace name' }),
      '$users',
    );
    await user.click(screen.getByRole('button', { name: 'Create namespace' }));

    expect(
      screen.getByText('Namespace names cannot start with $.'),
    ).toBeInTheDocument();
    expect(pushOps).not.toHaveBeenCalled();
  });

  it('rejects an existing namespace', async () => {
    const user = userEvent.setup();
    renderDialog();

    await user.type(
      screen.getByRole('textbox', { name: 'Namespace name' }),
      'todos',
    );
    await user.click(screen.getByRole('button', { name: 'Create namespace' }));

    expect(
      screen.getByText('That namespace already exists.'),
    ).toBeInTheDocument();
  });

  it('adds the id attribute and reports the new namespace', async () => {
    const user = userEvent.setup();
    const { onCreated, onOpenChange, pushOps } = renderDialog();

    await user.type(
      screen.getByRole('textbox', { name: 'Namespace name' }),
      'goals',
    );
    await user.click(screen.getByRole('button', { name: 'Create namespace' }));

    await waitFor(() => {
      expect(pushOps).toHaveBeenCalledTimes(1);
    });
    const calls = pushOps.mock.calls;
    expect(calls).toHaveLength(1);
    const [op, attribute] = calls[0]?.[0]?.[0] ?? [];
    expect(op).toBe('add-attr');
    expect(attribute).toMatchObject({
      'forward-identity': [expect.any(String), 'goals', 'id'],
    });
    expect(onCreated).toHaveBeenCalledWith('goals');
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });
});
