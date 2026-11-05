import { Link } from '@tanstack/react-router';
import { useId, useState } from 'react';
import { CopyButton } from '@/components/data/copy-button';
import { StatusPill } from '@/components/feedback/status-pill';
import { Button } from '@/components/ui/button';
import { Field, FieldDescription } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import {
  Sheet,
  SheetBody,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import type { Backup } from '@/lib/api/endpoints';
import {
  backupTime,
  restoreDefaultTitle,
  restoreJobPill,
} from '@/lib/backups/model';
import { useAppRestoreJobs, useRestoreBackup } from '@/lib/backups/queries';
import { toastManager } from '@/lib/toast';

type RestoreStep = 'target' | 'review' | 'confirm' | 'progress';

export interface RestoreBackupSheetProps {
  appId: string;
  appTitle: string;
  backup: Backup;
  onClose: () => void;
}

interface RestoreResult {
  appId: string;
  title: string;
  jobId: string;
}

const STEP_LABELS: Record<RestoreStep, string> = {
  target: 'Target',
  review: 'Review',
  confirm: 'Confirm',
  progress: 'Restore',
};

/**
 * C.15/D0.4 restore: choose the new app, review the consequences, type
 * `restore`, then follow the worker and link to the new app.
 */
export function RestoreBackupSheet({
  appId,
  appTitle,
  backup,
  onClose,
}: RestoreBackupSheetProps) {
  const [step, setStep] = useState<RestoreStep>('target');
  const [title, setTitle] = useState(() => restoreDefaultTitle(appTitle));
  const [draft, setDraft] = useState('');
  const [result, setResult] = useState<RestoreResult>();
  const restore = useRestoreBackup(appId);
  const restoreJobs = useAppRestoreJobs(appId);
  const titleId = useId();
  const confirmId = useId();

  const job = result
    ? restoreJobs.data?.['restore-jobs']?.find(
        (candidate) => candidate.id === result.jobId,
      )
    : undefined;
  const pill = job === undefined ? undefined : restoreJobPill(job);

  function startRestore(): void {
    if (result !== undefined) return;
    restore.mutate(
      { backupId: backup.id, title },
      {
        onSuccess: (created) => {
          setResult({
            appId: created.app.id,
            title: created.app.title,
            jobId: created.job.id,
          });
          setStep('progress');
          toastManager.add({ type: 'success', title: 'Restore started' });
        },
      },
    );
  }

  return (
    <Sheet
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <SheetContent size="md">
        <SheetHeader>
          <SheetTitle>Restore backup</SheetTitle>
          <SheetDescription>
            Step {stepNumber(step)} of 3 · {STEP_LABELS[step]} · snapshot from{' '}
            {backupTime(backup.backup_at)}
          </SheetDescription>
        </SheetHeader>

        <SheetBody className="flex flex-col gap-4">
          {step === 'target' ? (
            <div className="flex flex-col gap-4">
              <fieldset className="flex flex-col gap-2">
                <legend className="text-sm font-medium">Target</legend>
                <label
                  aria-label="New app"
                  className="border-input bg-muted flex items-start gap-2 rounded-md border p-3 text-sm"
                >
                  <input type="radio" name="restore-target" checked readOnly />
                  <span>
                    <span className="block font-medium">New app</span>
                    <span className="text-muted-foreground block text-xs">
                      The current app is never changed by a restore.
                    </span>
                  </span>
                </label>
              </fieldset>

              <Field>
                <label htmlFor={titleId} className="text-sm font-medium">
                  New app title
                </label>
                <Input
                  id={titleId}
                  value={title}
                  autoComplete="off"
                  onChange={(event) => {
                    setTitle(event.target.value);
                  }}
                />
                <FieldDescription>
                  You can rename the app after the restore finishes.
                </FieldDescription>
              </Field>
            </div>
          ) : null}

          {step === 'review' ? (
            <div className="flex flex-col gap-3">
              <p className="text-sm">
                Creates a new app with the data from{' '}
                <strong>{backupTime(backup.backup_at)}</strong>. The current app
                is not changed. Webhooks and OAuth clients are not copied.
              </p>
              <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
                <dt className="text-muted-foreground">Source</dt>
                <dd>{appTitle}</dd>
                <dt className="text-muted-foreground">New app</dt>
                <dd>{title.trim() === '' ? 'Untitled app' : title.trim()}</dd>
                <dt className="text-muted-foreground">Included</dt>
                <dd>Entities, files, schema and permissions</dd>
              </dl>
            </div>
          ) : null}

          {step === 'confirm' ? (
            <div className="flex flex-col gap-3">
              <ul className="text-muted-foreground flex list-disc flex-col gap-1 pl-5 text-sm">
                <li>Creates a new app in the same workspace.</li>
                <li>Copies the data and files from the snapshot.</li>
                <li>Leaves the current app untouched.</li>
              </ul>
              <div className="border-border bg-muted flex items-center justify-between gap-2 rounded-md border px-2.5 py-1.5">
                <code className="text-cell font-mono">restore</code>
                <CopyButton
                  value="restore"
                  aria-label="Copy confirmation text"
                />
              </div>
              <Field>
                <label htmlFor={confirmId} className="text-sm font-medium">
                  Type <span className="font-mono">restore</span> to confirm
                </label>
                <Input
                  id={confirmId}
                  value={draft}
                  autoComplete="off"
                  spellCheck={false}
                  onChange={(event) => {
                    setDraft(event.target.value);
                  }}
                />
                <FieldDescription>
                  The button unlocks when the text matches exactly.
                </FieldDescription>
              </Field>
              {restore.isError ? (
                <p role="alert" className="text-destructive text-sm">
                  {restore.error instanceof Error
                    ? restore.error.message
                    : 'Could not start the restore.'}
                </p>
              ) : null}
            </div>
          ) : null}

          {step === 'progress' && result !== undefined ? (
            <div className="flex flex-col gap-3">
              <p className="text-sm">
                Restoring {backupTime(backup.backup_at)} into{' '}
                <strong>{result.title}</strong>.
              </p>
              <div className="flex items-center gap-2 text-sm">
                {pill === undefined ? (
                  <span className="text-muted-foreground">Starting…</span>
                ) : (
                  <StatusPill status={pill.status} label={pill.label} />
                )}
                {job?.progress ? (
                  <span className="text-muted-foreground tabular-nums">
                    {job.progress}
                  </span>
                ) : null}
              </div>
              {job?.error ? (
                <p role="alert" className="text-destructive text-sm">
                  {job.error}
                </p>
              ) : null}
              <Link
                to="/apps/$appId/overview"
                params={{ appId: result.appId }}
                className="text-accent text-sm hover:underline"
              >
                Open {result.title}
              </Link>
            </div>
          ) : null}
        </SheetBody>

        <SheetFooter>
          {step === 'target' ? (
            <>
              <Button type="button" variant="secondary" onClick={onClose}>
                Cancel
              </Button>
              <Button
                type="button"
                disabled={title.trim() === ''}
                onClick={() => {
                  setStep('review');
                }}
              >
                Continue
              </Button>
            </>
          ) : null}
          {step === 'review' ? (
            <>
              <Button
                type="button"
                variant="secondary"
                onClick={() => {
                  setStep('target');
                }}
              >
                Back
              </Button>
              <Button
                type="button"
                onClick={() => {
                  setStep('confirm');
                }}
              >
                Continue
              </Button>
            </>
          ) : null}
          {step === 'confirm' ? (
            <>
              <Button
                type="button"
                variant="secondary"
                onClick={() => {
                  setStep('review');
                }}
              >
                Back
              </Button>
              <Button
                type="button"
                variant="destructive"
                disabled={draft !== 'restore' || restore.isPending}
                onClick={startRestore}
              >
                {restore.isPending ? 'Working…' : 'Restore backup'}
              </Button>
            </>
          ) : null}
          {step === 'progress' ? (
            <Button type="button" variant="secondary" onClick={onClose}>
              Close
            </Button>
          ) : null}
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}

function stepNumber(step: RestoreStep): number {
  return step === 'target' ? 1 : step === 'review' ? 2 : 3;
}
