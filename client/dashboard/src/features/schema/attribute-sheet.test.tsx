import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { ComponentProps } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { IndexingJob } from '@/lib/api/endpoints/apps';
import type {
  AppAttribute,
  AppInstantClient,
  AppNamespace,
  InstantAttr,
} from '@/lib/instant';
import { AttributeSheet } from './attribute-sheet';

const mocks = vi.hoisted(() => ({
  createIndexingJob:
    vi.fn<
      (appId: string, attrId: string, job: unknown) => Promise<IndexingJob>
    >(),
}));

vi.mock('@/lib/schema/jobs', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/lib/schema/jobs')>();
  return { ...actual, createIndexingJob: mocks.createIndexingJob };
});

const JOB: IndexingJob = {
  id: 'job-1',
  app_id: 'app-1',
  attr_id: 'attr-title',
  attr_name: 'todos.title',
  job_type: 'required',
  job_status: 'waiting',
  created_at: '2026-10-27T10:00:00Z',
  updated_at: '2026-10-27T10:00:00Z',
};

type PushOps = (ops: ReadonlyArray<ReadonlyArray<unknown>>) => Promise<unknown>;

function fakeClient(pushOps: PushOps): AppInstantClient {
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

const namespace: AppNamespace = {
  name: 'todos',
  isSystem: false,
  attributes: [],
  links: [],
};

const title: AppAttribute = {
  id: 'attr-title',
  namespace: 'todos',
  name: 'title',
  type: 'string',
  isIndexed: false,
  isUnique: false,
  isRequired: false,
  isPrimary: false,
};

const rawTitle: InstantAttr = {
  id: 'attr-title',
  'forward-identity': ['ident-title', 'todos', 'title'],
  cardinality: 'one',
  'value-type': 'blob',
  'checked-data-type': 'string',
  'index?': false,
  'unique?': false,
};

function renderSheet(
  overrides: Partial<ComponentProps<typeof AttributeSheet>> = {},
) {
  const pushOps = vi.fn<PushOps>(() => Promise.resolve(undefined));
  const onOpenChange = vi.fn();
  const onJobsStarted = vi.fn();
  render(
    <AttributeSheet
      open
      onOpenChange={onOpenChange}
      appId="app-1"
      namespace={namespace}
      attribute={null}
      rawAttribute={undefined}
      existingNames={['title']}
      client={fakeClient(pushOps)}
      onJobsStarted={onJobsStarted}
      {...overrides}
    />,
  );
  return { pushOps, onOpenChange, onJobsStarted };
}

beforeEach(() => {
  mocks.createIndexingJob.mockReset();
  mocks.createIndexingJob.mockResolvedValue(JOB);
});

describe('attribute sheet', () => {
  it('adds a new attribute in one step', async () => {
    const user = userEvent.setup();
    const { pushOps, onOpenChange } = renderSheet();

    await user.type(
      screen.getByRole('textbox', { name: 'Attribute name' }),
      'dueAt',
    );
    await user.click(screen.getByRole('button', { name: 'Add attribute' }));

    await waitFor(() => {
      expect(pushOps).toHaveBeenCalledTimes(1);
    });
    const [op, attribute] = pushOps.mock.calls[0]?.[0]?.[0] ?? [];
    expect(op).toBe('add-attr');
    expect(attribute).toMatchObject({
      'forward-identity': [expect.any(String), 'todos', 'dueAt'],
      'checked-data-type': 'string',
      'required?': false,
    });
    expect(onOpenChange).toHaveBeenCalledWith(false);
    expect(mocks.createIndexingJob).not.toHaveBeenCalled();
  });

  it('rejects a name the plan does not allow', async () => {
    const user = userEvent.setup();
    const { pushOps } = renderSheet();

    await user.type(
      screen.getByRole('textbox', { name: 'Attribute name' }),
      '$users',
    );
    await user.click(screen.getByRole('button', { name: 'Add attribute' }));

    expect(screen.getByRole('alert')).toHaveTextContent(
      'Names cannot start with $.',
    );
    expect(pushOps).not.toHaveBeenCalled();
  });

  it('starts a job for a changed constraint on edit', async () => {
    const user = userEvent.setup();
    const { onJobsStarted, pushOps } = renderSheet({
      attribute: title,
      rawAttribute: rawTitle,
    });

    await user.click(screen.getByRole('switch', { name: 'Required' }));
    await user.click(screen.getByRole('button', { name: 'Save attribute' }));

    await waitFor(() => {
      expect(mocks.createIndexingJob).toHaveBeenCalledWith(
        'app-1',
        'attr-title',
        { jobType: 'required' },
      );
    });
    // The name did not change, so no rename was sent.
    expect(pushOps).not.toHaveBeenCalled();
    expect(onJobsStarted).toHaveBeenCalledWith([JOB]);
  });
});
