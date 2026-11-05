import { DashboardSelect } from '../DashboardSelect';
import { useEffect, useRef, useState } from 'react';
import { v4 } from 'uuid';
import {
  Button,
  Content,
  ScreenHeading,
  SubsectionHeading,
  TextInput,
} from '@/components/ui';
import {
  checkCreation,
  creationReceipt,
  creationRequest,
  creationTitle,
  readRecovery,
  recoveryRecord,
  requestError,
  type CreationDraft,
  type CreationKind,
  type CreationReceipt,
  type Recovery,
  type Request,
  type Target,
} from './model';
import type { OrgSummary } from '@/lib/types';
import '../workbench/workbench.css';
import './creation.css';

export function CreationWorkspace({
  kind,
  accountId,
  availableTargets,
  initialTarget = 'personal',
  request,
  onOpen,
  onCreated,
  onCancel,
  onBusyChange,
  compact = false,
  first = false,
}: {
  kind: CreationKind;
  accountId: string;
  availableTargets: Target[];
  initialTarget?: string;
  request: Request;
  onOpen: (receipt: CreationReceipt) => Promise<void>;
  onCreated: (receipt: CreationReceipt) => void;
  onCancel: () => void;
  onBusyChange?: (busy: boolean) => void;
  compact?: boolean;
  first?: boolean;
}) {
  const [name, setName] = useState(''),
    [targetId, setTargetId] = useState(initialTarget),
    [draft, setDraft] = useState<CreationDraft | null>(null);
  const [pending, setPending] = useState(false),
    [error, setError] = useState(''),
    [notice, setNotice] = useState(''),
    [created, setCreated] = useState<CreationReceipt | null>(null);
  const [recovery, setRecovery] = useState<Recovery | null>(null),
    [checked, setChecked] = useState(false),
    [missing, setMissing] = useState(false),
    [candidates, setCandidates] = useState<OrgSummary[]>([]),
    [newAttempt, setNewAttempt] = useState(false);
  const [ready, setReady] = useState(false);
  const mutationPending = useRef(false);
  const busy = useRef(false),
    alive = useRef(true),
    panel = useRef<HTMLElement | null>(null);
  const storageKey = `dashboard:creation:${accountId}:${kind}`;
  const target = availableTargets.find((t) => t.id === targetId);
  const targetGone = kind === 'application' && !target;
  useEffect(() => {
    alive.current = true;
    try {
      const saved = readRecovery(sessionStorage.getItem(storageKey));
      if (saved?.kind === kind) {
        setRecovery(saved);
        setName(saved.title);
        setTargetId(saved.orgId ?? 'personal');
      }
    } catch {
      setNotice(
        'Recovery across page reloads is unavailable in this browser session.',
      );
    }
    setReady(true);
    return () => {
      alive.current = false;
    };
  }, [storageKey, kind]);
  useEffect(() => {
    onBusyChange?.(pending);
    return () => onBusyChange?.(false);
  }, [pending, onBusyChange]);
  useEffect(() => {
    if (draft || recovery || created) panel.current?.focus();
  }, [draft, recovery, created]);
  useEffect(() => {
    const guard = (event: BeforeUnloadEvent) => {
      if (mutationPending.current) {
        event.preventDefault();
        event.returnValue = '';
      }
    };
    window.addEventListener('beforeunload', guard);
    return () => window.removeEventListener('beforeunload', guard);
  }, []);
  function persist(value: Recovery | null) {
    try {
      if (value) sessionStorage.setItem(storageKey, JSON.stringify(value));
      else sessionStorage.removeItem(storageKey);
    } catch {
      setNotice(
        'Recovery across page reloads is unavailable. Keep this page open until you have checked the result.',
      );
    }
  }
  function review() {
    try {
      if (targetGone) throw new Error('Choose an available workspace.');
      const title = creationTitle(kind, name);
      const prepared: CreationDraft =
        kind === 'application'
          ? {
              kind,
              title,
              id: v4(),
              adminToken: v4(),
              ...(targetId !== 'personal' ? { orgId: targetId } : {}),
            }
          : { kind, title };
      setDraft(prepared);
      void submit(prepared);
    } catch (e) {
      setError(requestError(e));
    }
  }
  async function submit(prepared?: CreationDraft) {
    const currentDraft = prepared ?? draft;
    if (!currentDraft || busy.current || targetGone) return;
    busy.current = true;
    setPending(true);
    setError('');
    mutationPending.current = true;
    let submitted = false;
    let attempt: Recovery | null = null;
    try {
      const operation = creationRequest(currentDraft);
      let orgIds: string[] = [];
      if (kind === 'organization') {
        const inventory = await request<{ orgs?: OrgSummary[] }>('/dash');
        if (!Array.isArray(inventory.orgs))
          throw new Error(
            'Could not read the organization list before creation. Try again.',
          );
        orgIds = inventory.orgs.map((o) => o.id);
      } else if (currentDraft.orgId) {
        const current = await request<{ org?: { role?: string } }>(
          `/dash/orgs/${encodeURIComponent(currentDraft.orgId)}`,
        );
        if (
          !['owner', 'admin', 'collaborator'].includes(current.org?.role ?? '')
        )
          throw new Error(
            'Your role can no longer create applications in this organization. Choose another workspace.',
          );
      }
      if (!alive.current) return;
      attempt = recoveryRecord(currentDraft, orgIds);
      persist(attempt);
      submitted = true;
      const response = await request(operation.path, operation.init);
      if (!alive.current) return;
      const result = creationReceipt(currentDraft, response);
      // The write is confirmed. Opening the app may perform a full-page navigation.
      mutationPending.current = false;
      setCreated(result);
      setRecovery(null);
      setDraft(null);
      persist(null);
      try {
        onCreated(result);
      } catch {
        // Analytics and list callbacks must not prevent opening a created app.
      }
      setNotice(
        'Created successfully. Open your workspace when you are ready.',
      );
    } catch (e) {
      if (!alive.current) return;
      setError(requestError(e));
      if (submitted) {
        // The server can commit an app before initialization or its response fails.
        setRecovery(attempt);
        setChecked(false);
        setMissing(false);
        setCandidates([]);
      }
    } finally {
      mutationPending.current = false;
      busy.current = false;
      if (alive.current) setPending(false);
    }
  }
  async function check() {
    if (!recovery || busy.current) return;
    busy.current = true;
    setPending(true);
    setError('');
    setChecked(false);
    try {
      const result = await checkCreation(request, recovery);
      if (!alive.current) return;
      setChecked(true);
      setMissing(!!result.missing);
      setCandidates(result.candidates ?? []);
      if (result.found) {
        setCreated(result.found);
        setRecovery(null);
        setDraft(null);
        persist(null);
      }
    } catch (e) {
      if (alive.current) setError(requestError(e));
    } finally {
      busy.current = false;
      if (alive.current) setPending(false);
    }
  }
  async function open(receipt: CreationReceipt) {
    if (busy.current) return;
    busy.current = true;
    setPending(true);
    setError('');
    try {
      await onOpen(receipt);
    } catch (e) {
      if (alive.current)
        setError(
          `The ${kind} exists, but navigation failed. Try opening it again. ${requestError(e)}`,
        );
    } finally {
      busy.current = false;
      if (alive.current) setPending(false);
    }
  }
  async function copyId() {
    if (!created) return;
    try {
      await navigator.clipboard.writeText(created.id);
      setNotice('ID copied.');
    } catch {
      setError('Copy failed. Select the visible ID and copy it manually.');
    }
  }
  if (!ready)
    return <p className="creation-loading">Loading creation workspace…</p>;
  if (!created && !recovery) {
    const FormContainer = compact ? 'div' : 'main';
    return (
      <FormContainer
        className={
          kind === 'organization'
            ? ''
            : 'my-auto grid h-full w-full place-items-center overflow-auto p-4'
        }
      >
        <form
          className={
            kind === 'organization' ? '' : 'flex max-w-md flex-col gap-4'
          }
          aria-label="Creation form"
          onSubmit={(event) => {
            event.preventDefault();
            if (!pending) review();
          }}
        >
          {kind === 'organization' ? (
            <SubsectionHeading className="pb-4">
              Create Organization
            </SubsectionHeading>
          ) : (
            <>
              <div
                className="mb-2 flex justify-center text-4xl"
                aria-hidden="true"
              >
                🔥
              </div>
              <ScreenHeading className="text-center">
                {first ? 'Create your first app' : 'Time for a new app?'}
              </ScreenHeading>
              {targetId !== 'personal' && target && (
                <Content className="w-full">
                  This app will be created in the{' '}
                  <strong className="dark:text-white">{target.title}</strong>{' '}
                  organization.
                </Content>
              )}
              <Content>What would you like to call it?</Content>
            </>
          )}
          <TextInput
            aria-label={
              kind === 'application' ? 'App name' : 'Organization name'
            }
            label={kind === 'organization' ? 'Name' : undefined}
            autoFocus
            value={name}
            onChange={setName}
            disabled={pending}
            placeholder={
              kind === 'application' ? 'Name your app' : 'My Organization'
            }
          />
          {kind === 'application' && availableTargets.length > 1 && (
            <label className="flex flex-col gap-1 text-sm">
              Workspace
              <DashboardSelect
                aria-label="Workspace"
                value={targetId}
                disabled={pending}
                onChange={(event) => setTargetId(event.target.value)}
              >
                {availableTargets.map((target) => (
                  <option key={target.id} value={target.id}>
                    {target.title}
                  </option>
                ))}
              </DashboardSelect>
            </label>
          )}
          {error && (
            <p role="alert" className="text-sm text-red-600">
              {error}
            </p>
          )}
          {notice && (
            <p
              role="status"
              className="text-sm text-gray-500 dark:text-neutral-400"
            >
              {notice}
            </p>
          )}
          {kind === 'organization' ? (
            <div className="flex justify-end gap-2 pt-4">
              <Button variant="subtle" disabled={pending} onClick={onCancel}>
                Cancel
              </Button>
              <Button type="submit" loading={pending} disabled={!name.trim()}>
                Create
              </Button>
            </div>
          ) : (
            <>
              <Button
                type="submit"
                loading={pending}
                disabled={!name.trim() || targetGone}
              >
                Let's go!
              </Button>
              <Button
                type="button"
                variant="secondary"
                disabled={pending}
                onClick={onCancel}
              >
                Nevermind
              </Button>
            </>
          )}
        </form>
      </FormContainer>
    );
  }
  const Container = compact ? 'section' : 'main';
  return (
    <Container className={`creation-page ${compact ? 'creation-compact' : ''}`}>
      <header className="creation-header">
        <p className="dashboard-resource-eyebrow">
          {first ? 'Get started' : 'Workspace'} / Create {kind}
        </p>
        <h1>{first ? 'Create your first application' : `Create an ${kind}`}</h1>
        <p>
          {kind === 'application'
            ? 'Name your application and choose its workspace.'
            : 'Bring your applications and team into a shared workspace.'}
        </p>
      </header>
      <div className="creation-layout">
        <section
          className="creation-panel"
          ref={panel}
          tabIndex={-1}
          aria-label="Creation form"
        >
          {error && (
            <p className="creation-error" role="alert">
              {error}
            </p>
          )}
          {notice && (
            <p className="creation-notice" role="status">
              {notice}
            </p>
          )}
          {created ? (
            <>
              <p className="creation-step">
                {created.recovered
                  ? 'Application found'
                  : 'Created successfully'}
              </p>
              <h2>{created.title}</h2>
              <p>
                {created.recovered
                  ? 'This application exists. Open it to inspect its current configuration; a recovered record does not confirm that every setup step completed.'
                  : `Your ${kind} is ready to open.`}
              </p>
              <dl>
                <div>
                  <dt>
                    {kind === 'application' ? 'Application' : 'Organization'} ID
                  </dt>
                  <dd>
                    <code>{created.id}</code>
                  </dd>
                </div>
              </dl>
              <div className="wb-actions">
                <Button disabled={pending} onClick={() => open(created)}>
                  {pending ? 'Opening…' : `Open ${kind}`}
                </Button>
                <Button variant="secondary" onClick={copyId}>
                  Copy ID
                </Button>
              </div>
            </>
          ) : recovery ? (
            <>
              <p className="creation-step">Check previous attempt</p>
              <h2>{recovery.title}</h2>
              <p>
                The previous creation request may have completed. Check its
                result before submitting again.
              </p>
              <dl>
                <div>
                  <dt>Attempt started</dt>
                  <dd>{recovery.startedAt}</dd>
                </div>
                {recovery.id && (
                  <div>
                    <dt>Application ID</dt>
                    <dd>
                      <code>{recovery.id}</code>
                    </dd>
                  </div>
                )}
              </dl>
              <Button disabled={pending} onClick={check}>
                {pending ? 'Checking…' : 'Check creation result'}
              </Button>
              {checked && kind === 'application' && missing && (
                <p>
                  No application was found at this ID. A previous request may
                  still be finishing. Check again, or retry the same ID while
                  this page retains its prepared credentials.
                </p>
              )}
              {checked && missing && draft?.adminToken && (
                <Button
                  variant="secondary"
                  disabled={pending}
                  onClick={() => void submit()}
                >
                  Retry this application ID
                </Button>
              )}
              {checked && kind === 'organization' && (
                <>
                  <h3>Possible matches</h3>
                  <p>
                    {candidates.length
                      ? 'These organizations appeared since the attempt and have the same name. Open one to inspect it; the list cannot identify which request created it.'
                      : 'No new organization with this name is currently visible. This does not prove that the previous request failed.'}
                  </p>
                  {candidates.map((o) => (
                    <button
                      className="creation-candidate"
                      disabled={pending}
                      key={o.id}
                      onClick={() =>
                        open({
                          kind: 'organization',
                          id: o.id,
                          title: o.title,
                          recovered: true,
                        })
                      }
                    >
                      <strong>{o.title}</strong>
                      <code>{o.id}</code>
                      <span>Open organization ↗</span>
                    </button>
                  ))}
                </>
              )}
              {checked && (
                <div className="creation-new-attempt">
                  <label className="creation-checkbox">
                    <input
                      type="checkbox"
                      checked={newAttempt}
                      onChange={(e) => setNewAttempt(e.target.checked)}
                      disabled={pending}
                    />
                    I checked the result and want to start a separate creation.
                    This may create a duplicate.
                  </label>
                  <Button
                    variant="secondary"
                    disabled={pending || !newAttempt}
                    onClick={() => {
                      setRecovery(null);
                      setDraft(null);
                      persist(null);
                      setError('');
                      setNewAttempt(false);
                    }}
                  >
                    Start a new review
                  </Button>
                </div>
              )}
              <button
                className="creation-cancel"
                disabled={pending}
                onClick={onCancel}
              >
                {first ? 'Account settings' : 'Return to dashboard'}
              </button>
            </>
          ) : draft && !pending ? (
            <>
              <p className="creation-step">02 / Review</p>
              <h2>{draft.title}</h2>
              <dl>
                <div>
                  <dt>Workspace</dt>
                  <dd>
                    {kind === 'organization'
                      ? 'New organization · you become an owner'
                      : (target?.title ?? 'Unavailable')}
                  </dd>
                </div>
                {draft.id && (
                  <div>
                    <dt>Application ID</dt>
                    <dd>
                      <code>{draft.id}</code>
                    </dd>
                  </div>
                )}
              </dl>
              <p>
                {kind === 'application'
                  ? 'Creates one application in the selected workspace. Its admin credential will be available in App settings after creation.'
                  : 'Creates an organization with your account as its first owner. Invite people after it has been created.'}
              </p>
              {targetGone && (
                <p className="creation-error">
                  This workspace is no longer available. Go back and choose
                  another workspace.
                </p>
              )}
              <div className="wb-actions">
                <Button
                  variant="secondary"
                  disabled={pending}
                  onClick={() => {
                    setDraft(null);
                    setError('');
                  }}
                >
                  Back to details
                </Button>
                <Button
                  disabled={pending || targetGone}
                  onClick={() => void submit()}
                >
                  {pending ? 'Creating…' : `Create ${kind}`}
                </Button>
              </div>
            </>
          ) : (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                review();
              }}
            >
              <label>
                {kind === 'application' ? 'Application' : 'Organization'} name
                <input
                  disabled={pending}
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  autoComplete="off"
                  required
                  placeholder={
                    kind === 'application'
                      ? 'e.g. Customer portal'
                      : 'e.g. Product operations'
                  }
                />
              </label>
              {kind === 'organization' && (
                <p className="creation-hint">
                  Up to 140 characters. You can rename it later.
                </p>
              )}
              {kind === 'application' && (
                <label>
                  Workspace
                  <DashboardSelect
                    disabled={pending}
                    value={targetId}
                    onChange={(e) => setTargetId(e.target.value)}
                  >
                    {targetGone && (
                      <option value={targetId}>Workspace unavailable</option>
                    )}
                    {availableTargets.map((t) => (
                      <option key={t.id} value={t.id}>
                        {t.title}
                      </option>
                    ))}
                  </DashboardSelect>
                </label>
              )}
              <div className="wb-actions">
                <Button
                  type="submit"
                  disabled={pending || !name.trim() || targetGone}
                >
                  {pending ? 'Creating…' : `Create ${kind}`}
                </Button>
                <Button
                  variant="secondary"
                  disabled={pending}
                  onClick={onCancel}
                >
                  {first ? 'Account settings' : 'Cancel'}
                </Button>
              </div>
            </form>
          )}
        </section>
        <aside className="creation-guide">
          <p className="creation-step">What happens next</p>
          {kind === 'application' ? (
            <>
              <h2>A workspace for your data</h2>
              <ol>
                <li>
                  <strong>Connect your application</strong>
                  <p>Use the application ID with an SDK or the API.</p>
                </li>
                <li>
                  <strong>Define data and access</strong>
                  <p>
                    Inspect the schema and configure permissions for your
                    application.
                  </p>
                </li>
                <li>
                  <strong>Explore and verify</strong>
                  <p>
                    Use the data editor and Query Console to inspect your
                    records.
                  </p>
                </li>
              </ol>
            </>
          ) : (
            <>
              <h2>One place for your team</h2>
              <ol>
                <li>
                  <strong>Invite people</strong>
                  <p>Choose collaborator, administrator, or owner access.</p>
                </li>
                <li>
                  <strong>Add applications</strong>
                  <p>
                    Create applications in the organization or transfer existing
                    ones from App settings.
                  </p>
                </li>
                <li>
                  <strong>Review shared usage</strong>
                  <p>Inspect organization usage and member access.</p>
                </li>
              </ol>
            </>
          )}
        </aside>
      </div>
    </Container>
  );
}
