import type { OauthApp } from '@/lib/api/endpoints';
import { cn } from '@/lib/utils';

export interface OauthAppLogoProps {
  app: Pick<OauthApp, 'appName' | 'appLogo'>;
  size?: 'sm' | 'lg';
  className?: string;
}

/**
 * The uploaded logo, or the first letter of the name. The image is decorative
 * because the name is always next to it (C.14).
 */
export function OauthAppLogo({
  app,
  size = 'sm',
  className,
}: OauthAppLogoProps) {
  const box = size === 'lg' ? 'size-12 text-2xl' : 'size-8 text-sm';

  if (app.appLogo === null || app.appLogo === '') {
    return (
      <span
        aria-hidden="true"
        className={cn(
          'bg-muted text-muted-foreground flex shrink-0 items-center justify-center rounded-md font-semibold',
          box,
          className,
        )}
      >
        {app.appName.trim().charAt(0).toUpperCase() || 'O'}
      </span>
    );
  }

  return (
    <img
      src={app.appLogo}
      alt=""
      className={cn(
        'border-border bg-card shrink-0 rounded-md border object-contain',
        box,
        className,
      )}
    />
  );
}
