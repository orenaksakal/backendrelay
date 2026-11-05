import { createFileRoute } from '@tanstack/react-router';
import { OAuthCallback } from '../routes';

export const Route = createFileRoute('/dash/oauth/callback')({
  component: OAuthCallback,
  ssr: false,
});
