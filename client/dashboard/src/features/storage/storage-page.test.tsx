import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import type { AppInstantClient } from '@/lib/instant';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { render } from '@/test/render';

const mocks = vi.hoisted(() => {
  const files = [
    {
      id: 'file-avatar',
      path: 'avatar.png',
      size: 220_000,
      'content-type': 'image/png',
      url: 'https://cdn.example.test/avatar.png',
      serverCreatedAt: '2026-10-27T10:00:00Z',
    },
    {
      id: 'file-cat',
      path: 'avatars/2026/cat.png',
      size: 2048,
      'content-type': 'image/png',
      url: 'https://cdn.example.test/cat.png',
    },
    {
      id: 'file-dog',
      path: 'avatars/dog.png',
      size: 4096,
      'content-type': 'image/png',
      url: 'https://cdn.example.test/dog.png',
    },
    {
      id: 'file-report',
      path: 'docs/report.pdf',
      size: 9000,
      'content-type': 'application/pdf',
      url: 'https://cdn.example.test/report.pdf',
    },
  ];

  return {
    files,
    useFiles: vi.fn(),
    useFile: vi.fn(),
    uploadStorageFile: vi.fn(() => Promise.resolve(undefined)),
    deleteStorageFiles: vi.fn(() => Promise.resolve(undefined)),
  };
});

vi.mock('@instantdb/react', () => ({ init: () => ({}) }));

vi.mock('@/lib/instant', async () => {
  const actual =
    await vi.importActual<typeof import('@/lib/instant')>('@/lib/instant');
  const client = {
    core: {
      shutdown: () => undefined,
      subscribeQuery: () => () => undefined,
      _reactor: {
        subscribeAttrs: () => () => undefined,
        pushOps: () => Promise.resolve(undefined),
      },
    },
    tx: {},
    transact: () => Promise.resolve(undefined),
  } satisfies AppInstantClient;
  return {
    ...actual,
    useAppClient: () => client,
    peekAppClient: () => client,
  };
});

vi.mock('@/lib/storage/rows', () => ({
  useFiles: mocks.useFiles,
  useFile: mocks.useFile,
}));

vi.mock('@/lib/storage/api', () => ({
  uploadStorageFile: mocks.uploadStorageFile,
  deleteStorageFiles: mocks.deleteStorageFiles,
}));

function filesList(overrides: Record<string, unknown> = {}) {
  return {
    files: mocks.files,
    hasNextPage: false,
    isLoading: false,
    error: null,
    pending: false,
    paused: false,
    volumePaused: false,
    togglePause: vi.fn(),
    resume: vi.fn(),
    refresh: vi.fn(),
    ...overrides,
  };
}

beforeEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  window.sessionStorage.clear();
  vi.clearAllMocks();
  setSession({ token: mockSignInToken, user: mockSignInUser });
  mocks.useFiles.mockReturnValue(filesList());
  mocks.useFile.mockImplementation((_client: unknown, fileId: string) => ({
    file: mocks.files.find((file) => file.id === fileId) ?? null,
    isLoading: false,
    error: null,
  }));
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  window.sessionStorage.clear();
});

async function renderStorage(route = '/apps/app-1/storage') {
  const result = render({ route });
  await waitFor(() => {
    const heading = screen.queryByRole('heading', {
      level: 1,
      name: 'Storage',
    });
    const dialog = screen.queryByRole('dialog');
    expect(heading ?? dialog).not.toBeNull();
  });
  return result;
}

describe('storage page', () => {
  it('shows folder rows before the files in the open folder', async () => {
    const user = userEvent.setup();
    const { router } = await renderStorage();

    const table = await screen.findByRole('table', { name: 'Files' });
    expect(
      within(table).getByRole('button', { name: 'avatars' }),
    ).toBeInTheDocument();
    expect(
      within(table).getByRole('button', { name: 'avatar.png' }),
    ).toBeInTheDocument();
    expect(within(table).queryByText('cat.png')).not.toBeInTheDocument();
    expect(screen.getByText(/2 folders/)).toBeInTheDocument();

    await user.click(within(table).getByRole('button', { name: 'avatars' }));

    await waitFor(() => {
      expect(router.state.location.search).toMatchObject({
        prefix: 'avatars/',
      });
    });
  });

  it('searches the paths below the open folder', async () => {
    await renderStorage('/apps/app-1/storage?q=cat');

    const table = await screen.findByRole('table', { name: 'Files' });
    expect(
      within(table).getByRole('button', { name: 'cat.png' }),
    ).toBeInTheDocument();
    expect(
      within(table).queryByRole('button', { name: 'avatar.png' }),
    ).not.toBeInTheDocument();
  });

  it('opens the file sheet with an accessible preview', async () => {
    await renderStorage('/apps/app-1/storage?file=file-avatar');

    const sheet = await screen.findByRole('dialog');
    expect(
      within(sheet).getByRole('heading', { name: 'avatar.png' }),
    ).toBeInTheDocument();
    expect(
      within(sheet).getByRole('img', { name: 'avatar.png' }),
    ).toHaveAttribute('src', 'https://cdn.example.test/avatar.png');
    expect(
      within(sheet).getByText('Links expire after 7 days.'),
    ).toBeInTheDocument();
    expect(
      within(sheet).getByRole('link', { name: 'Download' }),
    ).toHaveAttribute('href', 'https://cdn.example.test/avatar.png');
  });

  it('uploads a chosen file to the open folder', async () => {
    const user = userEvent.setup();
    await renderStorage('/apps/app-1/storage?prefix=avatars%2F');

    const input = screen.getByLabelText('Choose files to upload');
    const file = new File(['sample'], 'new.png', { type: 'image/png' });
    await user.upload(input, file);

    await waitFor(() => {
      expect(mocks.uploadStorageFile).toHaveBeenCalledWith(
        expect.objectContaining({
          appId: 'app-1',
          path: 'avatars/new.png',
          file,
        }),
      );
    });
  });

  it('uploads files dropped on the table', async () => {
    await renderStorage();
    const region = screen.getByRole('region', { name: 'Storage files' });
    const file = new File(['dropped'], 'dropped.png', { type: 'image/png' });

    fireEvent.drop(region, { dataTransfer: { files: [file] } });

    await waitFor(() => {
      expect(mocks.uploadStorageFile).toHaveBeenCalledWith(
        expect.objectContaining({ path: 'dropped.png', file }),
      );
    });
  });

  it('asks before replacing an existing file', async () => {
    const user = userEvent.setup();
    await renderStorage();

    const input = screen.getByLabelText('Choose files to upload');
    const file = new File(['replacement'], 'avatar.png', {
      type: 'image/png',
    });
    await user.upload(input, file);

    const prompt = await screen.findByRole('heading', {
      name: 'Replace existing file?',
    });
    expect(prompt).toBeInTheDocument();
    expect(mocks.uploadStorageFile).not.toHaveBeenCalled();

    await user.click(screen.getByRole('button', { name: 'Replace' }));

    await waitFor(() => {
      expect(mocks.uploadStorageFile).toHaveBeenCalledWith(
        expect.objectContaining({ path: 'avatar.png', file }),
      );
    });
  });

  it('deletes selected files after a tier 2 confirmation', async () => {
    const user = userEvent.setup();
    await renderStorage();

    await user.click(
      await screen.findByRole('checkbox', { name: 'Select avatar.png' }),
    );
    await user.click(screen.getByRole('button', { name: 'Delete' }));

    const title = await screen.findByRole('heading', {
      name: 'Delete this file?',
    });
    const dialog = title.closest('[role="dialog"]');
    expect(dialog).not.toBeNull();
    await user.click(
      within(dialog as HTMLElement).getByRole('button', {
        name: 'Delete file',
      }),
    );

    await waitFor(() => {
      expect(mocks.deleteStorageFiles).toHaveBeenCalledWith({
        appId: 'app-1',
        filenames: ['avatar.png'],
      });
    });
  });

  it('uploads immediately even with the obsolete lock preference', async () => {
    const user = userEvent.setup();
    window.localStorage.setItem('br.requireUnlock', 'true');
    await renderStorage();

    const file = new File(['sample'], 'new.txt', { type: 'text/plain' });
    await user.upload(screen.getByLabelText('Choose files to upload'), file);

    await waitFor(() => {
      expect(mocks.uploadStorageFile).toHaveBeenCalledWith(
        expect.objectContaining({ appId: 'app-1', path: 'new.txt', file }),
      );
    });
    expect(screen.queryByText(/Editing is locked/)).not.toBeInTheDocument();
  });

  it('stops below admin access and explains the role', async () => {
    await renderStorage('/apps/app-2/storage');

    expect(
      await screen.findByText("You don't have access to files"),
    ).toBeInTheDocument();
    expect(
      screen.getByText('The file list needs admin access to this app.'),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole('table', { name: 'Files' }),
    ).not.toBeInTheDocument();
  });
});
