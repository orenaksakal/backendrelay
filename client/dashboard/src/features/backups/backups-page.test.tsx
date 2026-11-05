import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import {
  mockActiveBackupJobs,
  mockEnabledBackupSchedule,
  mockTodoApp,
} from '@/test/fixtures/api/apps';
import { mockDash } from '@/test/msw/handlers';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ROUTE = '/apps/app-1/backups';

beforeEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  setSession({ token: mockSignInToken, user: mockSignInUser });
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
});

async function renderSnapshots() {
  const result = render({ route: ROUTE });
  await screen.findByRole('heading', { level: 1, name: 'Backups' });
  return result;
}

describe('backup snapshots', () => {
  it('lists the stored snapshots with their size and status', async () => {
    await renderSnapshots();

    const table = await screen.findByRole('table', { name: 'Backups' });
    expect(within(table).getByText('Nightly')).toBeInTheDocument();
    expect(within(table).getByText('Before release')).toBeInTheDocument();
    expect(within(table).getByText('Available')).toBeInTheDocument();
    expect(within(table).getByText('Retained')).toBeInTheDocument();
  });

  it('creates a backup with an optional description', async () => {
    const user = userEvent.setup();
    let created: unknown;
    server.use(
      http.post(`${API}/dash/apps/app-1/backups`, async ({ request }) => {
        created = await request.json();
        return HttpResponse.json({
          job: {
            id: 'backup-job-created',
            app_id: 'app-1',
            app_backup_id: null,
            job_status: 'waiting',
            description: 'Before migration',
            work_estimate: 100,
            work_completed: 0,
            created_at: '2026-10-27T09:00:00Z',
            updated_at: '2026-10-27T09:00:00Z',
            done_at: null,
          },
        });
      }),
    );

    await renderSnapshots();
    await user.click(screen.getByRole('button', { name: 'Create backup' }));

    const dialog = await screen.findByRole('dialog');
    await user.type(
      within(dialog).getByLabelText(/Description/),
      'Before migration',
    );
    await user.click(
      within(dialog).getByRole('button', { name: 'Create backup' }),
    );

    await waitFor(() => {
      expect(created).toEqual({ description: 'Before migration' });
    });
    expect(await screen.findByText('Backup started')).toBeInTheDocument();
  });

  it('shows a backup that finished after the snapshot list was loaded', async () => {
    const user = userEvent.setup();
    const finishedBackup = {
      id: 'backup-created',
      isn: null,
      backup_at: '2026-10-27T09:00:05Z',
      files_size: 2_048,
      db_size: 1_024,
      uncompressed_size: 4_096,
      description: 'Live snapshot',
      expires_at: '2026-11-26T09:00:05Z',
    };
    const runningJob = {
      id: 'backup-job-created',
      app_id: 'app-1',
      app_backup_id: null,
      job_status: 'processing',
      description: 'Live snapshot',
      work_estimate: 100,
      work_completed: 40,
      created_at: '2026-10-27T09:00:00Z',
      updated_at: '2026-10-27T09:00:01Z',
      done_at: null,
    };
    let created = false;
    let jobDone = false;
    let jobPolls = 0;

    server.use(
      http.get(`${API}/dash/apps/app-1/backups`, () =>
        HttpResponse.json({ backups: jobDone ? [finishedBackup] : [] }),
      ),
      http.get(`${API}/dash/apps/app-1/backup-jobs`, () => {
        if (!created || jobDone) return HttpResponse.json({ jobs: [] });
        jobPolls += 1;
        if (jobPolls > 1) {
          // The worker finished the backup between two polls.
          jobDone = true;
          return HttpResponse.json({ jobs: [] });
        }
        return HttpResponse.json({ jobs: [runningJob] });
      }),
      http.post(`${API}/dash/apps/app-1/backups`, () => {
        created = true;
        return HttpResponse.json({ job: runningJob });
      }),
    );

    await renderSnapshots();
    expect(await screen.findByText('No backups yet')).toBeInTheDocument();

    const createButtons = screen.getAllByRole('button', {
      name: 'Create backup',
    });
    const toolbarCreate = createButtons[0];
    if (toolbarCreate === undefined) {
      throw new Error('the toolbar Create backup button is missing');
    }
    await user.click(toolbarCreate);
    const dialog = await screen.findByRole('dialog');
    await user.type(
      within(dialog).getByLabelText(/Description/),
      'Live snapshot',
    );
    await user.click(
      within(dialog).getByRole('button', { name: 'Create backup' }),
    );
    expect(await screen.findByText('Backup started')).toBeInTheDocument();

    // The jobs query polls every two seconds, so the finished backup only
    // shows up after the completion refresh has run.
    const table = await screen.findByRole(
      'table',
      { name: 'Backups' },
      { timeout: 6_000 },
    );
    expect(within(table).getByText('Live snapshot')).toBeInTheDocument();
  }, 15_000);

  it('opens the download sheet with the archive file list', async () => {
    const user = userEvent.setup();
    await renderSnapshots();
    const table = await screen.findByRole('table', { name: 'Backups' });
    const row = within(table).getByText('Nightly').closest('tr');
    expect(row).not.toBeNull();
    if (row === null) return;

    await user.click(
      within(row).getByRole('button', { name: 'Actions for Nightly' }),
    );
    await user.click(await screen.findByRole('menuitem', { name: 'Download' }));

    const sheet = await screen.findByRole('dialog');
    const files = await within(sheet).findByRole('table', {
      name: 'Backup files',
    });
    expect(within(files).getByText('config.json')).toBeInTheDocument();
    expect(within(files).getByText('todos.jsonl')).toBeInTheDocument();
    expect(within(files).getByText('me.png')).toBeInTheDocument();
    expect(
      within(sheet).getByRole('button', { name: 'Copy CLI command' }),
    ).toBeInTheDocument();
  });

  it('restores a snapshot into a new app after typing restore', async () => {
    const user = userEvent.setup();
    let restored: { title?: string } | undefined;
    server.use(
      http.post(
        `${API}/dash/apps/app-1/backups/backup-1/restore`,
        async ({ request }) => {
          restored = (await request.json()) as { title?: string };
          return HttpResponse.json({
            app: { id: 'app-restored-new', title: 'todo-app (restored)' },
            job: { id: 'restore-job-created' },
          });
        },
      ),
    );

    await renderSnapshots();
    const table = await screen.findByRole('table', { name: 'Backups' });
    const row = within(table).getByText('Nightly').closest('tr');
    expect(row).not.toBeNull();
    if (row === null) return;

    await user.click(
      within(row).getByRole('button', { name: 'Actions for Nightly' }),
    );
    await user.click(await screen.findByRole('menuitem', { name: 'Restore' }));

    const sheet = await screen.findByRole('dialog');
    const title = within(sheet).getByLabelText('New app title');
    expect((title as HTMLInputElement).value).toMatch(
      /^todo-app \(restored \d{4}-\d{2}-\d{2}\)$/,
    );
    await user.click(within(sheet).getByRole('button', { name: 'Continue' }));
    expect(
      within(sheet).getByText(/The current app is not changed/),
    ).toBeInTheDocument();
    await user.click(within(sheet).getByRole('button', { name: 'Continue' }));

    await user.type(
      within(sheet).getByLabelText(/Type restore to confirm/),
      'restore',
    );
    await user.click(
      within(sheet).getByRole('button', { name: 'Restore backup' }),
    );

    await waitFor(() => {
      expect(restored?.title).toMatch(
        /^todo-app \(restored \d{4}-\d{2}-\d{2}\)$/,
      );
    });
    expect(
      await within(sheet).findByRole('link', {
        name: 'Open todo-app (restored)',
      }),
    ).toHaveAttribute('href', '/apps/app-restored-new/overview');
    expect(await screen.findByText('Restore started')).toBeInTheDocument();
  });
});

describe('backup schedule', () => {
  it('edits the cron preset and saves the schedule from the unsaved bar', async () => {
    const user = userEvent.setup();
    let saved: unknown;
    server.use(
      http.get(`${API}/dash/apps/app-1/backup-schedule`, () =>
        HttpResponse.json({ schedule: mockEnabledBackupSchedule }),
      ),
      http.put(
        `${API}/dash/apps/app-1/backup-schedule`,
        async ({ request }) => {
          saved = await request.json();
          return HttpResponse.json({
            schedule: {
              cron: '15 4 * * 4',
              enabled: true,
              next_run_at: '2026-10-29T04:15:00Z',
              last_enqueued_at: null,
            },
          });
        },
      ),
    );

    render({ route: `${ROUTE}?tab=schedule` });
    await screen.findByRole('heading', { level: 1, name: 'Backups' });

    const frequency = await screen.findByRole('combobox', {
      name: 'Frequency',
    });
    await user.click(frequency);
    await user.click(await screen.findByRole('option', { name: 'Weekly' }));

    const day = await screen.findByRole('combobox', { name: 'Day of week' });
    await user.click(day);
    await user.click(await screen.findByRole('option', { name: 'Thursday' }));

    const time = screen.getByLabelText('Time');
    fireEvent.change(time, { target: { value: '04:15' } });

    expect(screen.getByText('You have unsaved changes')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => {
      expect(saved).toEqual({ cron: '15 4 * * 4', enabled: true });
    });
    expect(
      await screen.findByText('Backup schedule saved'),
    ).toBeInTheDocument();
  });

  it('hides the schedule controls from collaborators', async () => {
    server.use(
      http.get(`${API}/dash`, () =>
        HttpResponse.json({
          ...mockDash,
          apps: [{ ...mockTodoApp, role: 'collaborator' }],
        }),
      ),
    );

    render({ route: `${ROUTE}?tab=schedule` });
    await screen.findByRole('heading', { level: 1, name: 'Backups' });
    expect(
      await screen.findByText(
        /An app or workspace administrator can configure automatic backups/,
      ),
    ).toBeInTheDocument();
    expect(
      screen.getByRole('switch', { name: 'Automatic backups' }),
    ).toHaveAttribute('aria-disabled', 'true');
  });
});

describe('backup jobs', () => {
  it('shows running jobs and cancels a backup after confirmation', async () => {
    const user = userEvent.setup();
    let cancelled = '';
    server.use(
      http.get(`${API}/dash/apps/app-1/backup-jobs`, () =>
        HttpResponse.json({ jobs: mockActiveBackupJobs }),
      ),
      http.delete(`${API}/dash/apps/app-1/backup-jobs/:jobId`, ({ params }) => {
        cancelled = String(params['jobId']);
        return HttpResponse.json({ id: cancelled });
      }),
    );

    render({ route: `${ROUTE}?tab=jobs` });
    await screen.findByRole('heading', { level: 1, name: 'Backups' });

    const table = await screen.findByRole('table', { name: 'Backup jobs' });
    expect(within(table).getByText('Before migration')).toBeInTheDocument();
    expect(within(table).getByText('Running')).toBeInTheDocument();
    expect(within(table).getByText('40%')).toBeInTheDocument();
    expect(
      within(table).getByText('todo-app (restored 2026-10-20)'),
    ).toBeInTheDocument();

    await user.click(
      within(table).getByRole('button', {
        name: 'Cancel backup job Before migration',
      }),
    );
    const confirm = await screen.findByRole('dialog');
    await user.click(
      within(confirm).getByRole('button', { name: 'Cancel backup job' }),
    );

    await waitFor(() => {
      expect(cancelled).toBe('backup-job-1');
    });
    expect(await screen.findByText('Backup cancelled')).toBeInTheDocument();
  });

  it('refuses the page to app members', async () => {
    server.use(
      http.get(`${API}/dash`, () =>
        HttpResponse.json({
          ...mockDash,
          apps: [{ ...mockTodoApp, role: 'app-member' }],
        }),
      ),
    );

    render({ route: ROUTE });
    expect(
      await screen.findByText("You don't have access to backups"),
    ).toBeInTheDocument();
    expect(screen.getByText(/needs the collaborator role/)).toBeInTheDocument();
  });
});
