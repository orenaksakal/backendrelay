import { QueryClientProvider } from '@tanstack/react-query';
import { render as rtlRender, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ToastProvider, Toaster } from '@/components/ui/toast';
import type { AppInstantClient } from '@/lib/instant';
import { toastManager } from '@/lib/toast';
import { createTestQueryClient } from '@/test/render';
import { DeletedTab } from './deleted-tab';

const mocks = vi.hoisted(() => ({
  pushOps:
    vi.fn<(ops: ReadonlyArray<ReadonlyArray<unknown>>) => Promise<unknown>>(),
}));

vi.mock('@instantdb/react', () => ({ init: () => ({}) }));

vi.mock('@/lib/schema/queries', async () => {
  const actual = await vi.importActual<typeof import('@/lib/schema/queries')>(
    '@/lib/schema/queries',
  );
  return {
    ...actual,
    useSoftDeletedAttributes: () => ({
      data: {
        // The dashboard API returns the row exactly as the catalog stores it.
        attrs: [
          {
            id: 'attr-notes',
            'forward-identity': [
              'ident-notes',
              'attr-notes_deleted$todos',
              'attr-notes_deleted$notes',
            ],
            'value-type': 'blob',
            cardinality: 'one',
            'deletion-marked-at': '2026-10-27T10:00:00Z',
          },
        ],
        'grace-period-days': 2,
      },
      isLoading: false,
      isError: false,
      refetch: () => Promise.resolve(),
    }),
  };
});

const client = {
  core: {
    _reactor: {
      pushOps: (ops: ReadonlyArray<ReadonlyArray<unknown>>) =>
        mocks.pushOps(ops),
    },
  },
} as unknown as AppInstantClient;

function renderTab() {
  rtlRender(
    <QueryClientProvider client={createTestQueryClient()}>
      <ToastProvider toastManager={toastManager}>
        <DeletedTab appId="app-1" client={client} />
        <Toaster />
      </ToastProvider>
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  mocks.pushOps.mockReset();
});

describe('deleted tab', () => {
  it('restores an attribute and names it without the catalog marker', async () => {
    mocks.pushOps.mockResolvedValue(undefined);
    renderTab();
    const user = userEvent.setup();

    await user.click(screen.getByRole('button', { name: 'Restore' }));

    expect(mocks.pushOps).toHaveBeenCalledWith([
      ['restore-attr', 'attr-notes'],
    ]);
    expect(await screen.findByText('todos.notes restored')).toBeInTheDocument();
  });

  it('reports a restore the server rejects', async () => {
    mocks.pushOps.mockRejectedValue(new Error('restore failed'));
    renderTab();
    const user = userEvent.setup();

    await user.click(screen.getByRole('button', { name: 'Restore' }));

    expect(
      (await screen.findAllByText('restore failed')).length,
    ).toBeGreaterThan(0);
  });
});
