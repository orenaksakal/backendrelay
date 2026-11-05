import { useEffect, useRef, useState } from 'react';
import { Button } from '@/components/ui';
import { CreationWorkspace } from './CreationWorkspace';
import { requestError, type CreationReceipt, type Request } from './model';
import { profileMeta, type ProfileMeta } from './profile';
import './creation.css';
export function OnboardingWorkspace({
  accountId,
  selfHosted,
  request,
  onOpen,
  onCreated,
  onCancel,
}: {
  accountId: string;
  selfHosted: boolean;
  request: Request;
  onOpen: (receipt: CreationReceipt) => Promise<void>;
  onCreated: (receipt: CreationReceipt, meta?: ProfileMeta) => void;
  onCancel: () => void;
}) {
  const [loading, setLoading] = useState(true),
    [loaded, setLoaded] = useState(false),
    [error, setError] = useState(''),
    [revision, setRevision] = useState(0);
  const [profile, setProfile] = useState<{ meta?: ProfileMeta } | null>(null),
    [apps, setApps] = useState<{ id: string; title: string }[]>([]),
    [newApp, setNewApp] = useState(false),
    [existingSearch, setExistingSearch] = useState(''),
    [limit, setLimit] = useState(25);
  const [heard, setHeard] = useState(''),
    [experience, setExperience] = useState(''),
    [pending, setPending] = useState(false);
  const busy = useRef(false),
    alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  useEffect(() => {
    const abort = new AbortController();
    setLoading(true);
    setLoaded(false);
    setError('');
    request<{
      apps: { id: string; title: string }[];
      profile?: { meta?: ProfileMeta };
    }>('/dash', { signal: abort.signal })
      .then((result) => {
        if (abort.signal.aborted) return;
        if (!Array.isArray(result.apps))
          throw new Error(
            'The application list is incomplete. Refresh to try again.',
          );
        setLoaded(true);
        setApps(result.apps);
        setProfile(result.profile ?? null);
        setLoading(false);
      })
      .catch((e) => {
        if (!abort.signal.aborted) {
          setError(requestError(e));
          setLoading(false);
        }
      });
    return () => abort.abort();
  }, [request, revision]);
  async function saveProfile() {
    if (busy.current) return;
    let meta: ProfileMeta;
    try {
      meta = profileMeta(heard, experience);
    } catch (e) {
      setError(requestError(e));
      return;
    }
    busy.current = true;
    setPending(true);
    setError('');
    try {
      await request('/dash/profiles', {
        method: 'POST',
        body: JSON.stringify({ meta }),
      });
      if (alive.current) setProfile({ meta });
    } catch (e) {
      if (alive.current) setError(requestError(e));
    } finally {
      busy.current = false;
      if (alive.current) setPending(false);
    }
  }
  async function openApp(app: { id: string; title: string }) {
    if (busy.current) return;
    busy.current = true;
    setPending(true);
    setError('');
    try {
      await onOpen({ ...app, kind: 'application' });
    } catch (e) {
      if (alive.current) setError(requestError(e));
    } finally {
      busy.current = false;
      if (alive.current) setPending(false);
    }
  }
  if (loading || !loaded)
    return (
      <main className="creation-page">
        <header className="creation-header">
          <p className="dashboard-resource-eyebrow">Get started</p>
          <h1>
            {loading ? 'Loading your workspace…' : 'Workspace unavailable'}
          </h1>
        </header>
        {error && (
          <p className="creation-error" role="alert">
            {error}
          </p>
        )}
        {!loading && (
          <Button onClick={() => setRevision((v) => v + 1)}>
            Refresh workspace
          </Button>
        )}
      </main>
    );
  const visibleApps = apps.filter((app) =>
    `${app.title} ${app.id}`
      .toLowerCase()
      .includes(existingSearch.trim().toLowerCase()),
  );
  if (apps.length && !newApp)
    return (
      <main className="creation-page">
        <header className="creation-header">
          <p className="dashboard-resource-eyebrow">Get started</p>
          <h1>Your applications are ready to open</h1>
          <p>Continue with an existing application or create another.</p>
        </header>
        <section className="creation-panel">
          {error && (
            <p className="creation-error" role="alert">
              {error}
            </p>
          )}
          <label>
            Find an application
            <input
              type="search"
              value={existingSearch}
              onChange={(e) => {
                setExistingSearch(e.target.value);
                setLimit(25);
              }}
              placeholder="Name or application ID"
            />
          </label>
          {visibleApps.slice(0, limit).map((app) => (
            <button
              className="creation-candidate"
              disabled={pending}
              key={app.id}
              onClick={() => openApp(app)}
            >
              <strong>{app.title}</strong>
              <code>{app.id}</code>
              <span>Open application ↗</span>
            </button>
          ))}
          {!visibleApps.length && <p>No matching applications.</p>}
          {visibleApps.length > limit && (
            <Button variant="secondary" onClick={() => setLimit((v) => v + 25)}>
              Show 25 more
            </Button>
          )}
          <Button
            variant="secondary"
            disabled={pending}
            onClick={() => {
              setNewApp(true);
              setError('');
            }}
          >
            Create another application
          </Button>
        </section>
      </main>
    );
  if (!selfHosted && !profile)
    return (
      <main className="creation-page">
        <header className="creation-header">
          <p className="dashboard-resource-eyebrow">Get started / 01</p>
          <h1>A little about you</h1>
          <p>
            Tell us how you found BackendRelay and your coding experience. Next,
            create your first application.
          </p>
        </header>
        <div className="creation-layout">
          <form
            className="creation-panel"
            onSubmit={(e) => {
              e.preventDefault();
              void saveProfile();
            }}
          >
            <fieldset disabled={pending} className="creation-profile-fields">
              <label>
                How did you hear about us?
                <input
                  value={heard}
                  onChange={(e) => setHeard(e.target.value)}
                  required
                  autoComplete="off"
                />
              </label>
              <fieldset>
                <legend>Coding experience</legend>
                {[
                  {
                    value: 'vibe-dev',
                    title: 'Building with AI tools',
                    description: 'I use AI tools to build applications.',
                  },
                  {
                    value: 'junior-dev',
                    title: 'Some coding experience',
                    description: 'I write code and am still learning.',
                  },
                  {
                    value: 'senior-dev',
                    title: 'Experienced developer',
                    description: 'I build software professionally.',
                  },
                ].map((option) => (
                  <label className="creation-experience" key={option.value}>
                    <input
                      type="radio"
                      name="experience"
                      value={option.value}
                      checked={experience === option.value}
                      onChange={() => setExperience(option.value)}
                    />
                    <span>
                      <strong>{option.title}</strong>
                      <small>{option.description}</small>
                    </span>
                  </label>
                ))}
              </fieldset>
              {error && (
                <p className="creation-error" role="alert">
                  {error}
                </p>
              )}
              <Button
                type="submit"
                disabled={!heard.trim() || !experience || pending}
              >
                {pending ? 'Saving…' : 'Continue to application'}
              </Button>
            </fieldset>
          </form>
          <aside className="creation-guide">
            <p className="creation-step">Two steps to your workspace</p>
            <h2>Start with the essentials</h2>
            <ol>
              <li>
                <strong>Your profile</strong>
                <p>Share your experience with the team.</p>
              </li>
              <li>
                <strong>Your application</strong>
                <p>Choose a name, review it, and create your application.</p>
              </li>
            </ol>
          </aside>
        </div>
      </main>
    );
  return (
    <CreationWorkspace
      kind="application"
      first={!apps.length}
      accountId={accountId}
      availableTargets={[
        { id: 'personal', title: 'Personal workspace', role: 'owner' },
      ]}
      request={request}
      onOpen={onOpen}
      onCreated={(result) => onCreated(result, profile?.meta)}
      onCancel={onCancel}
    />
  );
}
