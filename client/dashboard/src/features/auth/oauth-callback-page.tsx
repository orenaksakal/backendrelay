import { useRouter, type NavigateOptions } from '@tanstack/react-router';
import { Loader2 } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { buttonVariants } from '@/components/ui/button';
import { apiFetch } from '@/lib/api/client';
import { sessionEndpoints } from '@/lib/api/endpoints';
import { cliApproveTarget } from '@/lib/auth/login';
import { authErrorMessage, oauthCallbackTarget } from '@/lib/auth/oauth';
import { setSession } from '@/lib/auth/session';
import { AuthCard } from './auth-card';

/** `/dash/oauth/callback` search params (C.1). */
export interface OAuthCallbackSearch {
  code?: string;
  error?: string;
  ticket?: string;
}

const EXCHANGE_FALLBACK = "We couldn't complete Google sign-in. Try again.";

/**
 * C.1 Google callback. The code is exchanged exactly once (a second render
 * must not burn it), the session is stored, and the tab moves on to
 * `redirect_path` or to CLI approval when the sign-in started there.
 */
export function OAuthCallbackPage({ search }: { search: OAuthCallbackSearch }) {
  const router = useRouter();
  const [error, setError] = useState<string | undefined>(search.error);
  const started = useRef(false);

  useEffect(() => {
    if (search.error !== undefined && search.error !== '') {
      return;
    }

    if (search.code === undefined || search.code === '') {
      const target =
        search.ticket !== undefined && search.ticket !== ''
          ? cliApproveTarget(search.ticket)
          : '/login';
      void router.navigate({ href: target, replace: true } as NavigateOptions);
      return;
    }

    if (started.current) return;
    started.current = true;

    apiFetch(sessionEndpoints.exchangeOAuthCode, {
      body: { code: search.code },
    })
      .then((result) => {
        setSession({ token: result.token, user: result.user });
        void router.navigate({
          href: oauthCallbackTarget(search.ticket, result.redirect_path),
          replace: true,
        } as NavigateOptions);
      })
      .catch((caught: unknown) => {
        setError(authErrorMessage(caught, EXCHANGE_FALLBACK));
      });
  }, [router, search.code, search.error, search.ticket]);

  if (error !== undefined) {
    return (
      <AuthCard title="Google sign-in failed">
        <div className="flex flex-col gap-4">
          <p role="alert" className="text-destructive text-sm">
            {error}
          </p>
          <a href="/login" className={buttonVariants({ variant: 'secondary' })}>
            Back to sign in
          </a>
        </div>
      </AuthCard>
    );
  }

  return (
    <AuthCard title="Signing in to BackendRelay">
      <p
        role="status"
        className="text-muted-foreground flex items-center gap-2 text-sm"
      >
        <Loader2 className="animate-spin" aria-hidden="true" />
        Signing you in…
      </p>
    </AuthCard>
  );
}
