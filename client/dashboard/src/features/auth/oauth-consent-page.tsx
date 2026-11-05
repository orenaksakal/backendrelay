import { useQuery } from '@tanstack/react-query';
import { useLocation } from '@tanstack/react-router';
import {
  AppWindow,
  Database,
  FolderOpen,
  Loader2,
  PenLine,
  Plus,
  Upload,
  type LucideIcon,
} from 'lucide-react';
import { buttonVariants } from '@/components/ui/button';
import { apiFetch, buildApiUrl } from '@/lib/api/client';
import { platformOauthEndpoints } from '@/lib/api/endpoints';
import { authErrorMessage } from '@/lib/auth/oauth';
import { consentScopeDescriptions } from '@/lib/auth/oauth-scopes';
import { signOut, useSession } from '@/lib/auth/session';
import { getRuntimeConfig } from '@/lib/runtime-config';
import { AuthCard } from './auth-card';
import { SignInPage } from './sign-in-page';

/** `/platform/oauth/start?redirect-id=` search params (C.2). */
export interface OAuthConsentSearch {
  'redirect-id'?: string;
}

/** Icon per scope group; the wording comes from `oauth-scopes.ts`. */
const SCOPE_ICONS: Record<string, LucideIcon> = {
  'apps-read': AppWindow,
  'apps-write': AppWindow,
  'apps-create': Plus,
  'data-read': Database,
  'data-write': PenLine,
  'storage-read': FolderOpen,
  'storage-write': Upload,
};

const CLAIM_FALLBACK = "This consent link isn't valid any more.";

/**
 * C.2 OAuth consent. When signed out it shows the sign-in card in place and
 * returns here afterwards; the claim runs only with a session.
 */
export function OAuthConsentPage({ search }: { search: OAuthConsentSearch }) {
  const session = useSession();
  const location = useLocation();
  const redirectId = search['redirect-id'];

  if (redirectId === undefined || redirectId === '') {
    return (
      <AuthCard title="This consent link isn't valid" width="wide">
        <div className="flex flex-col gap-4">
          <p role="alert" className="text-destructive text-sm">
            The request is missing its redirect ID. Start again from the app
            that sent you here.
          </p>
          <a href="/" className={buttonVariants({ variant: 'secondary' })}>
            Back to BackendRelay
          </a>
        </div>
      </AuthCard>
    );
  }

  if (session === null) {
    return <SignInPage search={{ redirect: location.href }} />;
  }

  return <ConsentCard redirectId={redirectId} />;
}

function ConsentCard({ redirectId }: { redirectId: string }) {
  const claim = useQuery({
    queryKey: ['platform-oauth', 'claim', redirectId],
    queryFn: ({ signal }) =>
      apiFetch(platformOauthEndpoints.claim, {
        body: { redirect: redirectId },
        signal,
      }),
    // The server claims a redirect once; a refetch would turn a valid consent
    // screen into "link isn't valid" while the user reads it.
    staleTime: Infinity,
    refetchOnMount: false,
    refetchOnWindowFocus: false,
    refetchOnReconnect: false,
    retry: false,
  });
  const config = getRuntimeConfig();

  if (claim.isPending) {
    return (
      <AuthCard title="Checking the request" width="wide">
        <p
          role="status"
          className="text-muted-foreground flex items-center gap-2 text-sm"
        >
          <Loader2 className="animate-spin" aria-hidden="true" />
          Loading the app's request…
        </p>
      </AuthCard>
    );
  }

  if (claim.isError) {
    return (
      <ConsentErrorCard
        message={authErrorMessage(claim.error, CLAIM_FALLBACK)}
      />
    );
  }

  if (config === undefined) {
    return (
      <ConsentErrorCard message="The dashboard configuration has not loaded yet." />
    );
  }

  const data = claim.data;

  const grantAction = buildApiUrl('/platform/oauth/grant', undefined, config);
  const denyAction = buildApiUrl('/platform/oauth/deny', undefined, config);

  return (
    <AuthCard
      width="wide"
      icon={<AppAvatar logo={data.appLogo} name={data.appName} />}
      title={`${data.appName} wants to access your BackendRelay account`}
    >
      <div className="flex flex-col gap-4">
        <p className="text-sm">
          Signed in as{' '}
          <strong className="font-semibold">{data.userEmail}</strong>.{' '}
          <button
            type="button"
            className="text-accent rounded-sm underline-offset-4 hover:underline"
            onClick={() => {
              signOut();
            }}
          >
            Not you? Sign out
          </button>
        </p>

        <div>
          <p className="text-sm">Allowing grants this app:</p>
          <ul className="mt-2 flex flex-col gap-2">
            {consentScopeDescriptions(data.scopes).map((scope) => {
              const Icon = SCOPE_ICONS[scope.id] ?? AppWindow;
              return (
                <li key={scope.id} className="flex items-start gap-2">
                  <Icon
                    aria-hidden="true"
                    className="text-muted-foreground mt-0.5 size-4 shrink-0"
                  />
                  <span className="text-sm">{scope.description}</span>
                </li>
              );
            })}
          </ul>
        </div>

        <p className="text-muted-foreground text-sm">
          You'll be redirected to{' '}
          <strong className="text-foreground font-semibold">
            {data.redirectOrigin}
          </strong>
          .
        </p>

        <div className="flex flex-wrap justify-end gap-2">
          <form method="post" action={denyAction}>
            <input type="hidden" name="redirect_id" value={redirectId} />
            <input type="hidden" name="grant_token" value={data.grantToken} />
            <button
              type="submit"
              className={buttonVariants({ variant: 'secondary' })}
            >
              Deny
            </button>
          </form>
          <form method="post" action={grantAction}>
            <input type="hidden" name="redirect_id" value={redirectId} />
            <input type="hidden" name="grant_token" value={data.grantToken} />
            <button type="submit" className={buttonVariants()}>
              Allow
            </button>
          </form>
        </div>
      </div>
    </AuthCard>
  );
}

/** The shared failure card for a claim that the server rejects (C.2). */
function ConsentErrorCard({ message }: { message: string }) {
  return (
    <AuthCard title="This consent link isn't valid" width="wide">
      <div className="flex flex-col gap-4">
        <p role="alert" className="text-destructive text-sm">
          {message}
        </p>
        <a href="/" className={buttonVariants({ variant: 'secondary' })}>
          Back to BackendRelay
        </a>
      </div>
    </AuthCard>
  );
}

/** The app logo, or its initial when the app has none (C.2). */
function AppAvatar({ logo, name }: { logo: string | null; name: string }) {
  if (logo !== null && logo !== '') {
    return (
      <img
        src={logo}
        alt=""
        width={32}
        height={32}
        className="border-border size-8 rounded-md border object-contain"
      />
    );
  }
  return (
    <span
      aria-hidden="true"
      className="bg-muted flex size-8 items-center justify-center rounded-md text-sm font-semibold"
    >
      {name.trim().slice(0, 1).toUpperCase()}
    </span>
  );
}
