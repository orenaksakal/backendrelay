import { createFileRoute } from '@tanstack/react-router';
import { TokensPage } from '@/features/account/tokens-page';

/** C.18 personal access tokens. */
export const Route = createFileRoute('/_authed/account/tokens')({
  component: TokensPage,
});
