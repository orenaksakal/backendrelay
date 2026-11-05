import { createFileRoute } from '@tanstack/react-router';
import { PlatformOAuthStart } from '../routes';

export const Route = createFileRoute('/platform/oauth/start')({
  component: PlatformOAuthStart,
  ssr: false,
});
