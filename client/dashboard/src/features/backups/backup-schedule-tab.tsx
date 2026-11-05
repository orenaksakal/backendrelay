import { useId, useState } from 'react';
import { ErrorState } from '@/components/feedback/error-state';
import { CardSkeleton } from '@/components/feedback/skeletons';
import { SectionCard } from '@/components/layout/section-card';
import { SettingsRow } from '@/components/layout/settings-row';
import { UnsavedChangesBar } from '@/components/layout/unsaved-changes-bar';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Switch } from '@/components/ui/switch';
import { isApiError } from '@/lib/api/client';
import type { BackupSchedule } from '@/lib/api/endpoints';
import {
  BACKUP_PRESET_LABELS,
  BACKUP_PRESETS,
  WEEKDAY_LABELS,
  cronDescription,
  cronForPreset,
  cronTiming,
  formatLocalDateTime,
  formatUtcDateTime,
  nextBackupRuns,
  presetForCron,
  scheduleDraft,
  scheduleDraftEqual,
  type BackupPreset,
  type CronTiming,
} from '@/lib/backups/model';
import { useBackupSchedule, useSetBackupSchedule } from '@/lib/backups/queries';
import { toastManager } from '@/lib/toast';

interface ScheduleEditor extends CronTiming {
  preset: BackupPreset;
  customCron: string;
  enabled: boolean;
}

function editorFor(schedule: BackupSchedule | null): ScheduleEditor {
  const draft = scheduleDraft(schedule);
  return {
    ...cronTiming(draft.cron),
    preset: presetForCron(draft.cron),
    customCron: draft.cron,
    enabled: draft.enabled,
  };
}

function editorCron(editor: ScheduleEditor): string {
  return cronForPreset(editor.preset, editor, editor.customCron);
}

export interface BackupScheduleTabProps {
  appId: string;
  canEdit: boolean;
}

/** C.15 schedule tab: the cron presets, the next runs preview and one save. */
export function BackupScheduleTab({ appId, canEdit }: BackupScheduleTabProps) {
  const schedule = useBackupSchedule(appId);

  if (schedule.isPending) {
    return <CardSkeleton />;
  }

  if (schedule.isError && schedule.data === undefined) {
    return (
      <ErrorState
        thing="the backup schedule"
        status={isApiError(schedule.error) ? schedule.error.status : undefined}
        message={
          schedule.error instanceof Error ? schedule.error.message : undefined
        }
        type={isApiError(schedule.error) ? schedule.error.type : undefined}
        hint={isApiError(schedule.error) ? schedule.error.hint : undefined}
        requestId={
          isApiError(schedule.error) ? schedule.error.requestId : undefined
        }
        onRetry={() => {
          void schedule.refetch();
        }}
      />
    );
  }

  const value = schedule.data.schedule ?? null;
  return (
    <ScheduleForm
      key={`${value?.cron ?? 'none'}:${String(value?.enabled ?? false)}`}
      appId={appId}
      schedule={value}
      canEdit={canEdit}
    />
  );
}

function ScheduleForm({
  appId,
  schedule,
  canEdit,
}: {
  appId: string;
  schedule: BackupSchedule | null;
  canEdit: boolean;
}) {
  const [editor, setEditor] = useState<ScheduleEditor>(() =>
    editorFor(schedule),
  );
  const [saveError, setSaveError] = useState<string>();
  const save = useSetBackupSchedule(appId);
  const frequencyId = useId();
  const timeId = useId();
  const minuteId = useId();
  const dayId = useId();
  const cronId = useId();
  const enabledLabelId = useId();

  const cron = editorCron(editor);
  const current = scheduleDraft(schedule);
  const dirty = !scheduleDraftEqual({ cron, enabled: editor.enabled }, current);

  let preview: string[] = [];
  let previewError: string | undefined;
  try {
    preview = nextBackupRuns(cron);
  } catch (error) {
    previewError =
      error instanceof Error ? error.message : 'Enter a valid cron expression.';
  }

  function saveSchedule(): void {
    setSaveError(undefined);
    if (previewError !== undefined) {
      setSaveError(previewError);
      return;
    }
    save.mutate(
      { cron, enabled: editor.enabled },
      {
        onSuccess: () => {
          toastManager.add({
            type: 'success',
            title: editor.enabled
              ? 'Backup schedule saved'
              : 'Backup schedule paused',
          });
        },
        onError: (error) => {
          setSaveError(
            error instanceof Error
              ? error.message
              : 'Could not save the schedule.',
          );
        },
      },
    );
  }

  function setPreset(preset: BackupPreset): void {
    if (preset === 'hourly') {
      setEditor((previous) => ({ ...previous, preset, minute: 0 }));
      return;
    }
    setEditor((previous) => ({ ...previous, preset }));
  }

  return (
    <div className="flex flex-col gap-4">
      <SectionCard
        title="Automatic backups"
        description="Create recovery points on a recurring schedule."
      >
        <div className="flex flex-col gap-4">
          <SettingsRow
            label="Automatic backups"
            labelId={enabledLabelId}
            description={
              editor.enabled
                ? 'The schedule below runs in UTC.'
                : 'Paused: no new snapshots are created by the schedule.'
            }
          >
            <Switch
              aria-labelledby={enabledLabelId}
              checked={editor.enabled}
              disabled={!canEdit}
              onCheckedChange={(checked) => {
                setEditor((previous) => ({ ...previous, enabled: checked }));
              }}
            />
          </SettingsRow>

          <SettingsRow
            label="Frequency"
            htmlFor={canEdit ? frequencyId : undefined}
            description="Choose a preset or enter a five-field cron expression."
          >
            <Select
              value={editor.preset}
              disabled={!canEdit}
              onValueChange={(value) => {
                if (BACKUP_PRESETS.includes(value as BackupPreset)) {
                  setPreset(value as BackupPreset);
                }
              }}
            >
              <SelectTrigger id={frequencyId} aria-label="Frequency">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {BACKUP_PRESETS.map((preset) => (
                  <SelectItem key={preset} value={preset}>
                    {BACKUP_PRESET_LABELS[preset]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </SettingsRow>

          {editor.preset === 'hourly' ? (
            <SettingsRow
              label="Minute"
              htmlFor={canEdit ? minuteId : undefined}
              description="Runs at this minute of every hour, in UTC."
            >
              <Input
                id={minuteId}
                type="number"
                min={0}
                max={59}
                className="w-24"
                disabled={!canEdit}
                value={editor.minute}
                onChange={(event) => {
                  const minute = Number(event.target.value);
                  if (Number.isInteger(minute) && minute >= 0 && minute <= 59) {
                    setEditor((previous) => ({ ...previous, minute }));
                  }
                }}
              />
            </SettingsRow>
          ) : null}

          {editor.preset === 'daily' || editor.preset === 'weekly' ? (
            <SettingsRow
              label="Time"
              htmlFor={canEdit ? timeId : undefined}
              description="Times are UTC; the preview also shows your local time."
            >
              <Input
                id={timeId}
                type="time"
                className="w-32"
                disabled={!canEdit}
                value={`${String(editor.hour).padStart(2, '0')}:${String(editor.minute).padStart(2, '0')}`}
                onChange={(event) => {
                  const [hourText, minuteText] = event.target.value.split(':');
                  const hour = Number(hourText);
                  const minute = Number(minuteText);
                  if (
                    Number.isInteger(hour) &&
                    hour >= 0 &&
                    hour <= 23 &&
                    Number.isInteger(minute) &&
                    minute >= 0 &&
                    minute <= 59
                  ) {
                    setEditor((previous) => ({ ...previous, hour, minute }));
                  }
                }}
              />
            </SettingsRow>
          ) : null}

          {editor.preset === 'weekly' ? (
            <SettingsRow
              label="Day"
              htmlFor={canEdit ? dayId : undefined}
              description="Every week on this day."
            >
              <Select
                value={String(editor.weekday)}
                disabled={!canEdit}
                onValueChange={(value) => {
                  const weekday = Number(value);
                  if (
                    Number.isInteger(weekday) &&
                    weekday >= 0 &&
                    weekday <= 6
                  ) {
                    setEditor((previous) => ({ ...previous, weekday }));
                  }
                }}
              >
                <SelectTrigger id={dayId} aria-label="Day of week">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {WEEKDAY_LABELS.map((label, weekday) => (
                    <SelectItem key={label} value={String(weekday)}>
                      {label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </SettingsRow>
          ) : null}

          {editor.preset === 'custom' ? (
            <SettingsRow
              label="Cron expression"
              htmlFor={canEdit ? cronId : undefined}
              description="Five fields: minute, hour, day of month, month, day of week (0–6, Sunday = 0)."
            >
              <Input
                id={cronId}
                className="w-48 font-mono"
                disabled={!canEdit}
                spellCheck={false}
                autoComplete="off"
                value={editor.customCron}
                onChange={(event) => {
                  setEditor((previous) => ({
                    ...previous,
                    customCron: event.target.value,
                  }));
                }}
              />
            </SettingsRow>
          ) : null}

          <div className="border-border flex flex-col gap-2 rounded-md border p-3">
            <p className="text-sm font-medium">Next runs</p>
            <p className="text-muted-foreground text-xs">
              {cronDescription(cron)} · <code>{cron}</code>
            </p>
            {previewError === undefined ? (
              <ul className="flex flex-col gap-1 text-sm">
                {preview.map((run) => (
                  <li key={run} className="flex flex-wrap gap-x-3">
                    <span>{formatUtcDateTime(run)}</span>
                    <span className="text-muted-foreground">
                      {formatLocalDateTime(run)} local
                    </span>
                  </li>
                ))}
              </ul>
            ) : (
              <p role="alert" className="text-destructive text-sm">
                {previewError}
              </p>
            )}
            <p className="text-muted-foreground text-xs">
              Missed runs are combined into one backup. A run is skipped while
              another backup is active.
            </p>
          </div>

          {!canEdit ? (
            <p className="text-muted-foreground text-sm">
              An app or workspace administrator can configure automatic backups.
            </p>
          ) : null}

          {schedule !== null ? (
            <div className="text-muted-foreground flex flex-wrap items-center gap-3 text-xs">
              <span>
                {schedule.enabled ? 'Enabled' : 'Paused'} · next run{' '}
                {formatUtcDateTime(schedule.next_run_at)}
              </span>
              {schedule.last_enqueued_at === null ? null : (
                <span>
                  Last queued {formatUtcDateTime(schedule.last_enqueued_at)}
                </span>
              )}
              <Button
                type="button"
                variant="link"
                size="sm"
                disabled={!canEdit || save.isPending}
                onClick={() => {
                  setEditor((previous) => ({
                    ...previous,
                    enabled: !previous.enabled,
                  }));
                }}
              >
                {editor.enabled ? 'Pause schedule' : 'Resume schedule'}
              </Button>
            </div>
          ) : null}
        </div>
      </SectionCard>

      <UnsavedChangesBar
        dirty={canEdit && dirty}
        saving={save.isPending}
        error={saveError ?? null}
        onReset={() => {
          setEditor(editorFor(schedule));
          setSaveError(undefined);
        }}
        onSave={saveSchedule}
      />
    </div>
  );
}
