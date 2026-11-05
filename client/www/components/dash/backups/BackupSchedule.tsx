import { DashboardSelect } from '../DashboardSelect';
import { useEffect, useRef, useState } from 'react';
import { Button } from '@/components/ui';

export type BackupScheduleValue = {
  cron: string;
  enabled: boolean;
  next_run_at: string;
  last_enqueued_at: string | null;
};
const presets = [
  ['0 * * * *', 'Hourly'],
  ['0 0 * * *', 'Daily'],
  ['0 0 * * 0', 'Weekly'],
  ['custom', 'Custom cron'],
];
export function BackupSchedule({
  schedule,
  loading,
  error,
  canEdit = true,
  onSave,
  onRetry,
  preview = false,
}: {
  schedule: BackupScheduleValue | null;
  loading?: boolean;
  error?: boolean;
  canEdit?: boolean;
  onSave: (value: { cron: string; enabled: boolean }) => Promise<void>;
  onRetry?: () => void;
  preview?: boolean;
}) {
  const heading = useRef<HTMLHeadingElement>(null);
  const errorSummary = useRef<HTMLParagraphElement>(null);
  const saveStatus = useRef<HTMLParagraphElement>(null);
  const [editing, setEditing] = useState(false);
  const [cron, setCron] = useState('0 0 * * *');
  const [preset, setPreset] = useState('0 0 * * *');
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState('');
  const [saveError, setSaveError] = useState('');
  useEffect(() => {
    if (saving) return;
    if (saveError) errorSummary.current?.focus();
    else if (message) saveStatus.current?.focus();
  }, [saving, saveError, message]);
  const scheduleDescription =
    schedule?.cron === '0 * * * *'
      ? 'Every hour, at :00 UTC'
      : schedule?.cron === '0 0 * * *'
        ? 'Every day at 00:00 UTC'
        : schedule?.cron === '0 0 * * 0'
          ? 'Every Sunday at 00:00 UTC'
          : 'Custom schedule (UTC)';
  async function save(value: { cron: string; enabled: boolean }) {
    if (saving) return;
    setSaving(true);
    setSaveError('');
    setMessage('');
    try {
      await onSave(value);
      setEditing(false);
      setMessage(
        preview
          ? 'Preview schedule updated. No backups will run.'
          : value.enabled
            ? 'Backup schedule saved.'
            : 'Backup schedule paused.',
      );
    } catch (e: any) {
      setSaveError(
        e?.body?.message ??
          e?.message ??
          'Could not save the schedule. Try again.',
      );
    } finally {
      setSaving(false);
    }
  }
  function edit() {
    const value = schedule?.cron ?? '0 0 * * *';
    setCron(value);
    setPreset(presets.some(([key]) => key === value) ? value : 'custom');
    setSaveError('');
    setMessage('');
    setEditing(true);
  }
  return (
    <section
      className="backup-schedule"
      aria-labelledby="backup-schedule-heading"
    >
      <div className="wb-toolbar">
        <div>
          <h2 ref={heading} tabIndex={-1} id="backup-schedule-heading">
            Automated backups
          </h2>
          <p className="wb-muted">
            Create recovery points on a recurring schedule.
          </p>
        </div>
        {!loading && !error && (
          <span className="wb-tag">
            {schedule?.enabled
              ? 'Enabled'
              : schedule
                ? 'Paused'
                : 'Not configured'}
          </span>
        )}
      </div>
      {loading ? (
        <p role="status">Loading schedule…</p>
      ) : error ? (
        <div role="alert">
          Could not load the backup schedule.{' '}
          <button className="wb-text-link" onClick={onRetry}>
            Retry
          </button>
        </div>
      ) : (
        <>
          {preview && (
            <p className="wb-muted">
              Interactive preview · schedules here do not run backups.
            </p>
          )}
          {editing && canEdit ? (
            <form
              onSubmit={(event) => {
                event.preventDefault();
                void save({ cron, enabled: schedule?.enabled ?? true });
              }}
            >
              <fieldset disabled={saving} className="backup-schedule-fields">
                <label>
                  Frequency
                  <DashboardSelect
                    autoFocus
                    value={preset}
                    onChange={(event) => {
                      setPreset(event.target.value);
                      if (event.target.value !== 'custom')
                        setCron(event.target.value);
                    }}
                  >
                    {presets.map(([value, label]) => (
                      <option key={value} value={value}>
                        {label}
                      </option>
                    ))}
                  </DashboardSelect>
                </label>
                {preset === 'custom' ? (
                  <label>
                    Cron expression
                    <input
                      required
                      maxLength={128}
                      value={cron}
                      onChange={(event) => setCron(event.target.value)}
                      aria-describedby="backup-cron-help"
                      placeholder="0 */6 * * *"
                      spellCheck={false}
                    />
                  </label>
                ) : (
                  <div className="backup-schedule-timing">
                    <span>Runs at</span>
                    <strong>
                      {preset === '0 * * * *'
                        ? 'Every hour, at :00'
                        : preset === '0 0 * * 0'
                          ? 'Sunday at 00:00'
                          : 'Every day at 00:00'}
                    </strong>
                  </div>
                )}
                <div className="backup-schedule-timing">
                  <span>Timezone</span>
                  <strong>UTC</strong>
                </div>
              </fieldset>
              <p id="backup-cron-help" className="wb-muted">
                {preset === 'custom'
                  ? 'Five fields: minute, hour, day of month, month, day of week (0–6, Sunday = 0). Example: 0 */6 * * * runs every six hours. '
                  : ''}
                Missed runs are combined into one backup. A run is skipped if
                another backup is active. Existing retention rules apply.
                Pausing leaves queued and running backups unchanged.
              </p>
              <div className="wb-actions">
                <Button size="mini" type="submit" disabled={saving}>
                  {saving ? 'Saving…' : 'Save schedule'}
                </Button>
                <button
                  type="button"
                  className="wb-text-link"
                  disabled={saving}
                  onClick={() => {
                    setEditing(false);
                    heading.current?.focus();
                    setSaveError('');
                  }}
                >
                  Cancel
                </button>
              </div>
            </form>
          ) : (
            <div className="backup-schedule-summary">
              <div>
                {schedule ? (
                  <>
                    <strong>{scheduleDescription}</strong>
                    <p className="wb-muted">
                      Cron: <code>{schedule.cron}</code>
                    </p>
                    <p>
                      {schedule.enabled
                        ? `Next scheduled run: ${new Date(schedule.next_run_at).toLocaleString('en-GB', { timeZone: 'UTC' })} UTC`
                        : 'Resume to schedule the next backup.'}
                    </p>
                    {schedule.last_enqueued_at && (
                      <p className="wb-muted">
                        Last queued:{' '}
                        {new Date(schedule.last_enqueued_at).toLocaleString(
                          'en-GB',
                          { timeZone: 'UTC' },
                        )}{' '}
                        UTC
                      </p>
                    )}
                  </>
                ) : (
                  <p>
                    Choose hourly, daily, weekly, or your own cron expression.
                  </p>
                )}
              </div>
              {canEdit && (
                <div className="wb-actions">
                  <Button
                    size="mini"
                    variant="secondary"
                    onClick={edit}
                    disabled={saving}
                  >
                    {schedule ? 'Edit schedule' : 'Configure schedule'}
                  </Button>
                  {schedule && (
                    <button
                      className="wb-text-link"
                      disabled={saving}
                      onClick={() =>
                        void save({
                          cron: schedule.cron,
                          enabled: !schedule.enabled,
                        })
                      }
                    >
                      {saving
                        ? 'Saving…'
                        : schedule.enabled
                          ? 'Pause schedule'
                          : 'Resume schedule'}
                    </button>
                  )}
                </div>
              )}
            </div>
          )}
          {!canEdit && (
            <p className="wb-muted">
              An app or workspace administrator can configure automated backups.
            </p>
          )}
        </>
      )}
      {saveError && (
        <p ref={errorSummary} tabIndex={-1} role="alert" className="wb-error">
          {saveError}
        </p>
      )}
      {message && (
        <p ref={saveStatus} tabIndex={-1} role="status">
          {message}
        </p>
      )}
    </section>
  );
}
