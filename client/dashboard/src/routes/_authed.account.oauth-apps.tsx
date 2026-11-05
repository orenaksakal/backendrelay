import { createFileRoute } from '@tanstack/react-router';
import { OAuthAppsPage } from '@/features/account/oauth-apps-page';

/** C.18 authorized OAuth apps. */
export const Route = createFileRoute('/_authed/account/oauth-apps')({
  component: OAuthAppsPage,
});
