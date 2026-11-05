import { useMutation } from '@tanstack/react-query';
import { useRouter, type NavigateOptions } from '@tanstack/react-router';
import { Loader2, LogOut } from 'lucide-react';
import { PageHeader } from '@/components/layout/page-header';
import { SectionCard } from '@/components/layout/section-card';
import { SettingsRow } from '@/components/layout/settings-row';
import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { accountErrorMessage } from '@/lib/account/errors';
import {
  PAGE_SIZE_OPTIONS,
  isPageSize,
  useAccountPreferences,
} from '@/lib/account/preferences';
import { apiFetch } from '@/lib/api/client';
import { sessionEndpoints } from '@/lib/api/endpoints';
import { signOut, useSession } from '@/lib/auth/session';
import { isTheme, useTheme } from '@/lib/theme';

const SIGN_OUT_ERROR = "Couldn't sign out. Try again.";

const THEME_OPTIONS = [
  { value: 'system', label: 'System' },
  { value: 'light', label: 'Light' },
  { value: 'dark', label: 'Dark' },
] as const;

/**
 * C.18 Profile: the signed-in address, browser preferences (theme, page size)
 * and the sign-out action.
 */
export function ProfilePage() {
  const router = useRouter();
  const session = useSession();
  const { theme, setTheme } = useTheme();
  const { defaultPageSize, setDefaultPageSize } = useAccountPreferences();

  const signOutMutation = useMutation({
    mutationFn: () => apiFetch(sessionEndpoints.signOut),
    onSuccess: () => {
      signOut();
      void router.navigate({ href: '/login' } as NavigateOptions);
    },
  });

  return (
    <div className="flex max-w-3xl flex-col gap-4">
      <PageHeader
        title="Profile"
        description="Your account and how the dashboard behaves in this browser."
      />

      <SectionCard
        title="Account"
        description="The address used to sign in. It cannot be changed here."
      >
        <SettingsRow label="Email">
          <span className="text-cell font-mono">
            {session?.user.email ?? '—'}
          </span>
        </SettingsRow>
      </SectionCard>

      <SectionCard title="Preferences" description="Saved in this browser.">
        <div className="flex flex-col gap-4">
          <SettingsRow
            label="Theme"
            labelId="theme-label"
            description="System follows your operating system setting."
          >
            <Select
              value={theme}
              onValueChange={(value) => {
                if (isTheme(value)) setTheme(value);
              }}
            >
              <SelectTrigger aria-labelledby="theme-label" className="w-32">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {THEME_OPTIONS.map((option) => (
                  <SelectItem key={option.value} value={option.value}>
                    {option.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </SettingsRow>

          <SettingsRow
            label="Default page size"
            labelId="page-size-label"
            description="Used by lists that show 25, 50, 100 or 200 rows."
          >
            <Select
              value={String(defaultPageSize)}
              onValueChange={(value) => {
                if (typeof value !== 'string') return;
                const parsed = Number(value);
                if (isPageSize(parsed)) setDefaultPageSize(parsed);
              }}
            >
              <SelectTrigger aria-labelledby="page-size-label" className="w-24">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {PAGE_SIZE_OPTIONS.map((size) => (
                  <SelectItem key={size} value={String(size)}>
                    {size}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </SettingsRow>
        </div>
      </SectionCard>

      <SectionCard
        title="Session"
        description="Signing out ends this browser session."
      >
        <div className="flex flex-wrap items-center justify-between gap-3">
          <p className="text-muted-foreground text-sm">
            You are signed in as{' '}
            <span className="text-foreground font-medium">
              {session?.user.email ?? 'this account'}
            </span>
            .
          </p>
          <Button
            type="button"
            variant="secondary"
            disabled={signOutMutation.isPending}
            aria-busy={signOutMutation.isPending}
            onClick={() => {
              signOutMutation.mutate();
            }}
          >
            {signOutMutation.isPending ? (
              <Loader2 className="animate-spin" aria-hidden="true" />
            ) : (
              <LogOut aria-hidden="true" />
            )}
            Sign out
          </Button>
        </div>
        {signOutMutation.isError ? (
          <p role="alert" className="text-destructive mt-3 text-sm">
            {accountErrorMessage(signOutMutation.error, SIGN_OUT_ERROR)}
          </p>
        ) : null}
      </SectionCard>
    </div>
  );
}
