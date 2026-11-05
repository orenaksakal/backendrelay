import { Link, useRouter, type NavigateOptions } from '@tanstack/react-router';
import { Diamond, Loader2 } from 'lucide-react';
import { useEffect, useId, useRef, useState } from 'react';
import { Button, buttonVariants } from '@/components/ui/button';
import {
  Field,
  FieldError,
  FieldLabel,
  FieldSeparator,
} from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { apiFetch } from '@/lib/api/client';
import { sessionEndpoints } from '@/lib/api/endpoints';
import {
  MAGIC_CODE_LENGTH,
  RESEND_COOLDOWN_SECONDS,
  normalizeMagicCode,
  oauthStartUrl,
  postSignInTarget,
  sendCodeErrorMessage,
  verifyCodeErrorMessage,
  type SignInSearch,
} from '@/lib/auth/login';
import { usePublicConfig } from '@/lib/auth/queries';
import { setSession } from '@/lib/auth/session';
import { getRuntimeConfig } from '@/lib/runtime-config';

const INCOMPLETE_CODE_MESSAGE = 'Enter the 6-digit code from the email.';

/**
 * C.1 `/login`: email, then a six-digit code. The card follows B.3 (400px,
 * centred on `--background`). Errors stay on the field they belong to; the
 * Google button appears only when the public config probe reports it, and never
 * in the embedded devtool (C.20).
 */
export function SignInPage({
  search,
  embedded = false,
}: {
  search: SignInSearch;
  /** Renders only the sign-in card so the SDK devtool can embed it (C.20). */
  embedded?: boolean;
}) {
  const router = useRouter();
  const publicConfig = usePublicConfig();
  const config = getRuntimeConfig();
  const googleUrl =
    publicConfig.data?.googleLogin === true && config !== undefined
      ? oauthStartUrl(config, search)
      : undefined;

  const [step, setStep] = useState<'email' | 'code'>('email');
  const [email, setEmail] = useState('');
  const [code, setCode] = useState('');
  const [error, setError] = useState<string | undefined>(undefined);
  const [pending, setPending] = useState(false);
  const [resendSeconds, setResendSeconds] = useState(0);
  const busy = useRef(false);
  const emailRef = useRef<HTMLInputElement>(null);
  const codeRef = useRef<HTMLInputElement>(null);

  const emailId = useId();
  const codeId = useId();
  const emailErrorId = `${emailId}-error`;
  const codeErrorId = `${codeId}-error`;

  useEffect(() => {
    if (step === 'email') {
      emailRef.current?.focus();
    } else {
      codeRef.current?.focus();
    }
  }, [step]);

  // The visible countdown is the disabled state of "Resend code" (C.1).
  useEffect(() => {
    if (resendSeconds <= 0) return;
    const timer = window.setTimeout(() => {
      setResendSeconds((seconds) => seconds - 1);
    }, 1000);
    return () => {
      window.clearTimeout(timer);
    };
  }, [resendSeconds]);

  async function sendCode(resend: boolean): Promise<void> {
    if (busy.current) return;
    busy.current = true;
    setPending(true);
    setError(undefined);
    try {
      await apiFetch(sessionEndpoints.sendMagicCode, {
        body: { email: email.trim() },
      });
      if (!resend) {
        setStep('code');
        setCode('');
      }
      setResendSeconds(RESEND_COOLDOWN_SECONDS);
    } catch (caught) {
      setError(sendCodeErrorMessage(caught));
    } finally {
      busy.current = false;
      setPending(false);
    }
  }

  async function verifyCode(candidate: string): Promise<void> {
    if (busy.current) return;
    const value = normalizeMagicCode(candidate);
    if (value.length !== MAGIC_CODE_LENGTH) {
      setError(INCOMPLETE_CODE_MESSAGE);
      return;
    }
    busy.current = true;
    setPending(true);
    setError(undefined);
    try {
      const result = await apiFetch(sessionEndpoints.verifyMagicCode, {
        body: { email: email.trim(), code: value },
      });
      setSession({ token: result.token, user: result.user });
      // The devtool signs in inline and stays on `/_devtool`.
      if (!embedded) {
        void router.navigate({
          href: postSignInTarget(search),
          replace: true,
        } as NavigateOptions);
      }
    } catch (caught) {
      setError(verifyCodeErrorMessage(caught));
    } finally {
      busy.current = false;
      setPending(false);
    }
  }

  const card = (
    <section
      aria-labelledby="sign-in-title"
      className="border-border bg-card rounded-lg border p-6"
    >
      <h1 id="sign-in-title" className="text-page">
        Sign in to BackendRelay
      </h1>

      {step === 'email' ? (
        <form
          className="mt-6 flex flex-col gap-4"
          onSubmit={(event) => {
            event.preventDefault();
            void sendCode(false);
          }}
        >
          {embedded || googleUrl === undefined ? null : (
            <>
              {/* A navigation, so it stays a link: Base UI's Button would
                      force the button role. */}
              <a
                href={googleUrl}
                className={buttonVariants({
                  variant: 'secondary',
                  size: 'lg',
                })}
              >
                <GoogleIcon />
                Continue with Google
              </a>
              <FieldSeparator>or</FieldSeparator>
            </>
          )}

          <Field invalid={error !== undefined}>
            <FieldLabel htmlFor={emailId}>Email</FieldLabel>
            <Input
              id={emailId}
              ref={emailRef}
              type="email"
              autoComplete="email"
              placeholder="you@example.com"
              value={email}
              aria-invalid={error !== undefined}
              aria-describedby={error === undefined ? undefined : emailErrorId}
              onChange={(event) => {
                setEmail(event.target.value);
                setError(undefined);
              }}
            />
            {error === undefined ? null : (
              <FieldError id={emailErrorId} match role="alert">
                {error}
              </FieldError>
            )}
          </Field>

          <Button
            type="submit"
            disabled={pending || email.trim() === ''}
            aria-busy={pending}
          >
            {pending ? (
              <Loader2 className="animate-spin" aria-hidden="true" />
            ) : null}
            Send code
          </Button>
        </form>
      ) : (
        <form
          className="mt-6 flex flex-col gap-4"
          onSubmit={(event) => {
            event.preventDefault();
            void verifyCode(code);
          }}
        >
          <p className="text-sm">
            We sent a 6-digit code to{' '}
            <strong className="font-semibold">{email.trim()}</strong>.
          </p>

          <Field invalid={error !== undefined}>
            <FieldLabel htmlFor={codeId}>Code</FieldLabel>
            <Input
              id={codeId}
              ref={codeRef}
              inputMode="numeric"
              autoComplete="one-time-code"
              autoCapitalize="off"
              spellCheck={false}
              value={code}
              aria-invalid={error !== undefined}
              aria-describedby={error === undefined ? undefined : codeErrorId}
              onChange={(event) => {
                const next = normalizeMagicCode(event.target.value);
                setCode(next);
                setError(undefined);
                if (next.length === MAGIC_CODE_LENGTH) {
                  void verifyCode(next);
                }
              }}
            />
            {error === undefined ? null : (
              <FieldError id={codeErrorId} match role="alert">
                {error}
              </FieldError>
            )}
          </Field>

          <div className="flex items-center justify-between gap-2">
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={() => {
                setStep('email');
                setCode('');
                setError(undefined);
              }}
            >
              Use a different email
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              disabled={pending || resendSeconds > 0}
              onClick={() => {
                void sendCode(true);
              }}
            >
              {resendSeconds > 0
                ? `Resend code in ${String(resendSeconds)}s`
                : 'Resend code'}
            </Button>
          </div>

          <p role="status" className="text-muted-foreground text-xs">
            {pending ? 'Verifying code…' : ''}
          </p>
        </form>
      )}
    </section>
  );

  if (embedded) {
    return <div className="w-full">{card}</div>;
  }

  return (
    <main
      id="main"
      tabIndex={-1}
      className="bg-background flex min-h-dvh flex-col items-center justify-center p-4 md:p-6"
    >
      <div className="w-full max-w-[400px]">
        <Link
          to="/"
          aria-label="BackendRelay home"
          className="text-foreground inline-flex h-8 items-center gap-2 rounded-md px-1.5 font-semibold"
        >
          <Diamond aria-hidden="true" className="size-4" />
          <span>BackendRelay</span>
        </Link>

        <div className="mt-4">{card}</div>
      </div>
    </main>
  );
}

/** The Google mark, copied from the old dashboard's sign-in button. */
function GoogleIcon() {
  return (
    <svg
      aria-hidden="true"
      viewBox="0 0 24 24"
      width="16"
      height="16"
      focusable="false"
    >
      <path
        d="M22.56 12.25c0-.78-.07-1.53-.2-2.25H12v4.26h5.92c-.26 1.37-1.04 2.53-2.21 3.31v2.77h3.57c2.08-1.92 3.28-4.74 3.28-8.09z"
        fill="#4285F4"
      />
      <path
        d="M12 23c2.97 0 5.46-.98 7.28-2.66l-3.57-2.77c-.98.66-2.23 1.06-3.71 1.06-2.86 0-5.29-1.93-6.16-4.53H2.18v2.84C3.99 20.53 7.7 23 12 23z"
        fill="#34A853"
      />
      <path
        d="M5.84 14.09c-.22-.66-.35-1.36-.35-2.09s.13-1.43.35-2.09V7.07H2.18C1.43 8.55 1 10.22 1 12s.43 3.45 1.18 4.93l2.85-2.22.81-.62z"
        fill="#FBBC05"
      />
      <path
        d="M12 5.38c1.62 0 3.06.56 4.21 1.64l3.15-3.15C17.45 2.09 14.97 1 12 1 7.7 1 3.99 3.47 2.18 7.07l3.66 2.84c.87-2.6 3.3-4.53 6.16-4.53z"
        fill="#EA4335"
      />
    </svg>
  );
}
