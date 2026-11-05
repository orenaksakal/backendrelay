import { useId, useMemo, useState } from 'react';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { ErrorState } from '@/components/feedback/error-state';
import { CardSkeleton } from '@/components/feedback/skeletons';
import { PageHeader } from '@/components/layout/page-header';
import { SectionCard } from '@/components/layout/section-card';
import { SettingsRow } from '@/components/layout/settings-row';
import { UnsavedChangesBar } from '@/components/layout/unsaved-changes-bar';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Input } from '@/components/ui/input';
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group';
import { Switch } from '@/components/ui/switch';
import { Textarea } from '@/components/ui/textarea';
import { isApiError } from '@/lib/api/client';
import type { KnownOperatorFlag } from '@/lib/api/endpoints';
import { creationErrorMessage } from '@/lib/creation/creation';
import { truncateId } from '@/lib/format';
import {
  SIGNUP_MODE_OPTIONS,
  advancedFlagRows,
  draftFromFlags,
  formatFlagValue,
  hasDraftProblems,
  isSignupMode,
  normaliseAdvancedChange,
  operatorDraftProblems,
  savePlanFromDraft,
  type AdvancedFlagChange,
  type AdvancedFlagRow,
  type OperatorSettingsDraft,
} from '@/lib/operator/model';
import { useOperatorFlags, useSaveOperatorFlags } from '@/lib/operator/queries';
import { toastManager } from '@/lib/toast';
import { useBootstrap } from '@/lib/workspace/queries';
import { AdvancedFlagSheet } from './advanced-flag-sheet';

const SAVE_ERROR = "Couldn't save the operator settings. Try again.";

/**
 * C.19 Operator settings: the typed deployment flags and an advanced table for
 * everything else. All controls write into one draft, so there is a single
 * unsaved-changes bar and Save writes only the values that changed (B.5.1).
 */
export function OperatorSettingsPage() {
  const flags = useOperatorFlags();

  if (flags.isError && flags.data === undefined) {
    return (
      <div className="flex max-w-4xl flex-col gap-4">
        <OperatorSettingsHeader />
        <ErrorState
          thing="operator settings"
          status={isApiError(flags.error) ? flags.error.status : undefined}
          message={
            flags.error instanceof Error ? flags.error.message : undefined
          }
          type={isApiError(flags.error) ? flags.error.type : undefined}
          hint={isApiError(flags.error) ? flags.error.hint : undefined}
          requestId={
            isApiError(flags.error) ? flags.error.requestId : undefined
          }
          onRetry={() => {
            void flags.refetch();
          }}
        />
      </div>
    );
  }

  if (flags.data === undefined) {
    return (
      <div className="flex max-w-4xl flex-col gap-4">
        <OperatorSettingsHeader />
        <CardSkeleton />
        <CardSkeleton />
        <CardSkeleton />
      </div>
    );
  }

  return (
    <OperatorSettingsForm
      key={JSON.stringify(flags.data.flags)}
      flags={flags.data.flags}
      known={flags.data.known}
    />
  );
}

function OperatorSettingsHeader() {
  return (
    <PageHeader
      title="Operator settings"
      description="Deployment-wide flags stored on the config app. Existing accounts and apps keep working."
    />
  );
}

interface OperatorSettingsFormProps {
  flags: Record<string, unknown>;
  known: ReadonlyArray<KnownOperatorFlag>;
}

interface AdvancedRow extends AdvancedFlagRow {
  staged: boolean;
}

function OperatorSettingsForm({ flags, known }: OperatorSettingsFormProps) {
  const bootstrap = useBootstrap();
  const save = useSaveOperatorFlags();
  const initialDraft = useMemo(() => draftFromFlags(flags), [flags]);
  const [draft, setDraft] = useState<OperatorSettingsDraft>(initialDraft);
  const [attempted, setAttempted] = useState(false);
  const [editing, setEditing] = useState<AdvancedFlagRow | undefined>();

  const problems = useMemo(() => operatorDraftProblems(draft), [draft]);
  const plan = useMemo(() => savePlanFromDraft(flags, draft), [flags, draft]);
  const invalid = hasDraftProblems(problems);
  const dirty = plan.writes.length > 0 || plan.resets.length > 0 || invalid;

  const signupLabelId = useId();
  const emailsId = useId();
  const emailsDescriptionId = `${emailsId}-description`;
  const ephemeralLabelId = useId();
  const rateId = useId();
  const expiryId = useId();
  const webhookIpsId = useId();
  const webhookIpsDescriptionId = `${webhookIpsId}-description`;
  const throttleId = useId();

  const throttleOptions = useMemo(() => {
    const options = (bootstrap.data?.apps ?? []).map((app) => ({
      id: app.id,
      title: app.title,
    }));
    const listed = new Set(options.map((option) => option.id));
    for (const appId of draft.throttledApps) {
      if (listed.has(appId)) continue;
      options.push({
        id: appId,
        title: `Unknown app ${truncateId(appId)}`,
      });
    }
    return options;
  }, [bootstrap.data?.apps, draft.throttledApps]);

  const advancedRows = useMemo<AdvancedRow[]>(
    () =>
      advancedFlagRows(known, flags).map((row) => {
        const change = draft.advanced[row.name];
        const value =
          change === undefined
            ? row.value
            : change.action === 'set'
              ? change.value
              : undefined;
        return { ...row, value, staged: change !== undefined };
      }),
    [draft.advanced, flags, known],
  );

  function update(patch: Partial<OperatorSettingsDraft>): void {
    setDraft((current) => ({ ...current, ...patch }));
    save.reset();
  }

  function toggleThrottled(appId: string, checked: boolean): void {
    const next = new Set(draft.throttledApps);
    if (checked) {
      next.add(appId);
    } else {
      next.delete(appId);
    }
    update({
      throttledApps: [...next].sort((left, right) => left.localeCompare(right)),
    });
  }

  function applyAdvanced(name: string, change: AdvancedFlagChange): void {
    setDraft((current) => {
      const advanced = { ...current.advanced };
      const normalised = normaliseAdvancedChange(flags[name], change);
      if (normalised === null) {
        Reflect.deleteProperty(advanced, name);
      } else {
        advanced[name] = normalised;
      }
      return { ...current, advanced };
    });
    save.reset();
    setEditing(undefined);
  }

  function resetDraft(): void {
    setDraft(initialDraft);
    setAttempted(false);
    save.reset();
  }

  function saveDraft(): void {
    setAttempted(true);
    if (invalid || !dirty || save.isPending) return;
    save.mutate(plan, {
      onSuccess: () => {
        toastManager.add({ type: 'success', title: 'Operator settings saved' });
      },
    });
  }

  const barError =
    attempted && invalid
      ? 'Fix the highlighted settings.'
      : save.isError
        ? creationErrorMessage(save.error, SAVE_ERROR)
        : null;

  const advancedColumns: SimpleTableColumn<AdvancedRow>[] = [
    {
      id: 'name',
      header: 'Flag',
      cell: (row) => (
        <div className="flex min-w-0 flex-col gap-0.5">
          <span className="font-mono text-xs">{row.name}</span>
          <span className="text-muted-foreground truncate text-xs">
            {row.description ?? 'Not documented'}
          </span>
        </div>
      ),
    },
    {
      id: 'value',
      header: 'Value',
      cell: (row) => {
        const text = formatFlagValue(row.value);
        return (
          <code
            title={text}
            className="block max-w-[24rem] truncate font-mono text-xs"
          >
            {text ?? 'unset'}
          </code>
        );
      },
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      align: 'right',
      cell: (row) => (
        <div className="flex items-center justify-end gap-2">
          {row.staged ? <Badge tone="warning">Unsaved</Badge> : null}
          <Button
            type="button"
            variant="ghost"
            size="sm"
            aria-label={`Edit ${row.name}`}
            onClick={() => {
              setEditing(row);
            }}
          >
            Edit
          </Button>
        </div>
      ),
    },
  ];

  return (
    <div className="flex max-w-4xl flex-col gap-4">
      <OperatorSettingsHeader />

      <SectionCard
        title="Sign-ups"
        description="Who can create a dashboard account on this deployment."
      >
        <div className="flex flex-col gap-4">
          <div className="flex flex-col gap-2">
            <span id={signupLabelId} className="text-sm font-medium">
              Who can sign up
            </span>
            <RadioGroup
              aria-labelledby={signupLabelId}
              value={draft.signupMode}
              className="gap-3"
              onValueChange={(value: unknown) => {
                if (isSignupMode(value)) update({ signupMode: value });
              }}
            >
              {SIGNUP_MODE_OPTIONS.map((option) => {
                const descriptionId = `signup-${option.value}-description`;
                return (
                  <div key={option.value} className="flex items-start gap-2">
                    <RadioGroupItem
                      value={option.value}
                      aria-label={option.label}
                      aria-describedby={descriptionId}
                    />
                    <div className="flex flex-col gap-0.5">
                      <span className="text-sm">{option.label}</span>
                      <span
                        id={descriptionId}
                        className="text-muted-foreground text-xs"
                      >
                        {option.description}
                      </span>
                    </div>
                  </div>
                );
              })}
            </RadioGroup>
          </div>

          {draft.signupMode === 'restricted' ? (
            <div className="flex flex-col gap-1.5">
              <label htmlFor={emailsId} className="text-sm font-medium">
                Allowed email addresses
              </label>
              <Textarea
                id={emailsId}
                rows={5}
                className="font-mono"
                spellCheck={false}
                value={draft.allowedEmailsText}
                aria-invalid={
                  attempted && problems.allowedEmails !== undefined
                    ? true
                    : undefined
                }
                aria-describedby={emailsDescriptionId}
                onChange={(event) => {
                  update({ allowedEmailsText: event.target.value });
                }}
              />
              <p
                id={emailsDescriptionId}
                className="text-muted-foreground text-xs"
              >
                One address per line. Only used while sign-ups are restricted.
              </p>
              {attempted && problems.allowedEmails !== undefined ? (
                <p
                  role="alert"
                  className="text-destructive text-xs font-medium"
                >
                  {problems.allowedEmails}
                </p>
              ) : null}
            </div>
          ) : null}
        </div>
      </SectionCard>

      <SectionCard
        title="Ephemeral apps"
        description="Temporary apps created by unauthenticated clients."
      >
        <SettingsRow
          label="Allow ephemeral app creation"
          labelId={ephemeralLabelId}
          description="When off, clients must sign in before they can create an app."
        >
          <Switch
            aria-labelledby={ephemeralLabelId}
            checked={draft.ephemeralApps}
            onCheckedChange={(checked) => {
              update({ ephemeralApps: checked });
            }}
          />
        </SettingsRow>
      </SectionCard>

      <SectionCard
        title="Magic codes"
        description="Sign-in code limits for every app unless the app overrides them."
      >
        <div className="flex flex-col gap-4">
          <SettingsRow
            label="Requests per hour"
            htmlFor={rateId}
            description="Per email address and app."
          >
            <div className="flex flex-col gap-1">
              <div className="flex items-center gap-2">
                <Input
                  id={rateId}
                  inputMode="numeric"
                  className="w-24"
                  value={draft.magicCodeRateLimit}
                  aria-invalid={
                    attempted && problems.magicCodeRateLimit !== undefined
                      ? true
                      : undefined
                  }
                  onChange={(event) => {
                    update({ magicCodeRateLimit: event.target.value });
                  }}
                />
                <span className="text-muted-foreground text-xs">per hour</span>
              </div>
              {attempted && problems.magicCodeRateLimit !== undefined ? (
                <p
                  role="alert"
                  className="text-destructive text-xs font-medium"
                >
                  {problems.magicCodeRateLimit}
                </p>
              ) : null}
            </div>
          </SettingsRow>

          <SettingsRow
            label="Default expiry"
            htmlFor={expiryId}
            description="Minutes. Apps can override it; the server caps codes at 60 minutes."
          >
            <div className="flex flex-col gap-1">
              <div className="flex items-center gap-2">
                <Input
                  id={expiryId}
                  inputMode="numeric"
                  className="w-24"
                  value={draft.magicCodeExpiry}
                  aria-invalid={
                    attempted && problems.magicCodeExpiry !== undefined
                      ? true
                      : undefined
                  }
                  onChange={(event) => {
                    update({ magicCodeExpiry: event.target.value });
                  }}
                />
                <span className="text-muted-foreground text-xs">minutes</span>
              </div>
              {attempted && problems.magicCodeExpiry !== undefined ? (
                <p
                  role="alert"
                  className="text-destructive text-xs font-medium"
                >
                  {problems.magicCodeExpiry}
                </p>
              ) : null}
            </div>
          </SettingsRow>
        </div>
      </SectionCard>

      <SectionCard
        title="Webhooks"
        description="Private destinations that the webhook SSRF guard may reach."
      >
        <div className="flex flex-col gap-1.5">
          <p className="text-warning text-xs">
            Only add addresses you control.
          </p>
          <label htmlFor={webhookIpsId} className="text-sm font-medium">
            SSRF allowlist IPs
          </label>
          <Textarea
            id={webhookIpsId}
            rows={4}
            className="font-mono"
            spellCheck={false}
            value={draft.webhookIpsText}
            aria-invalid={
              attempted && problems.webhookIps !== undefined ? true : undefined
            }
            aria-describedby={webhookIpsDescriptionId}
            onChange={(event) => {
              update({ webhookIpsText: event.target.value });
            }}
          />
          <p
            id={webhookIpsDescriptionId}
            className="text-muted-foreground text-xs"
          >
            One IPv4 or IPv6 address or CIDR per line. Every other private
            destination stays blocked.
          </p>
          {attempted && problems.webhookIps !== undefined ? (
            <p role="alert" className="text-destructive text-xs font-medium">
              {problems.webhookIps}
            </p>
          ) : null}
        </div>
      </SectionCard>

      <SectionCard
        title="Realtime"
        description="Group realtime refreshes for apps that burst after writes."
      >
        <div className="flex flex-col gap-4">
          <fieldset className="flex flex-col gap-2">
            <legend className="text-sm font-medium">Throttled apps</legend>
            <p className="text-muted-foreground text-xs">
              Refreshes for these apps wait for the throttle window before they
              are sent.
            </p>
            <div className="border-border max-h-56 overflow-y-auto rounded-md border">
              {throttleOptions.length === 0 ? (
                <p className="text-muted-foreground p-3 text-xs">
                  No apps are available in this workspace.
                </p>
              ) : (
                throttleOptions.map((option) => (
                  <div
                    key={option.id}
                    className="border-border flex min-h-9 items-center gap-2 border-b px-3 py-1 last:border-b-0"
                  >
                    <Checkbox
                      aria-label={`Throttle ${option.title}`}
                      checked={draft.throttledApps.includes(option.id)}
                      onCheckedChange={(checked) => {
                        toggleThrottled(option.id, checked);
                      }}
                    />
                    <span className="min-w-0 flex-1 truncate text-sm">
                      {option.title}
                    </span>
                    <span className="text-muted-foreground font-mono text-xs">
                      {truncateId(option.id)}
                    </span>
                  </div>
                ))
              )}
            </div>
          </fieldset>

          <SettingsRow
            label="Throttle window"
            htmlFor={throttleId}
            description="How long a grouped refresh waits before it is delivered."
          >
            <div className="flex flex-col gap-1">
              <div className="flex items-center gap-2">
                <Input
                  id={throttleId}
                  inputMode="numeric"
                  className="w-24"
                  value={draft.throttleMs}
                  aria-invalid={
                    attempted && problems.throttleMs !== undefined
                      ? true
                      : undefined
                  }
                  onChange={(event) => {
                    update({ throttleMs: event.target.value });
                  }}
                />
                <span className="text-muted-foreground text-xs">ms</span>
              </div>
              {attempted && problems.throttleMs !== undefined ? (
                <p
                  role="alert"
                  className="text-destructive text-xs font-medium"
                >
                  {problems.throttleMs}
                </p>
              ) : null}
            </div>
          </SettingsRow>
        </div>
      </SectionCard>

      <SectionCard
        title="Advanced"
        description="Every other deployment flag. Edit one value at a time as JSON."
      >
        <SimpleTable
          caption="Advanced flags"
          columns={advancedColumns}
          rows={advancedRows}
          getRowId={(row) => row.name}
          empty="No other flags are set."
        />
      </SectionCard>

      {editing === undefined ? null : (
        <AdvancedFlagSheet
          flag={editing}
          initialValue={flags[editing.name]}
          onApply={(change) => {
            applyAdvanced(editing.name, change);
          }}
          onClose={() => {
            setEditing(undefined);
          }}
        />
      )}

      <UnsavedChangesBar
        dirty={dirty}
        saving={save.isPending}
        error={barError}
        onReset={resetDraft}
        onSave={saveDraft}
      />
    </div>
  );
}
