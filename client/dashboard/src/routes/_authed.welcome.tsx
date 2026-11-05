import { createFileRoute } from '@tanstack/react-router';
import { WelcomePage } from '@/features/creation/welcome-page';

/** C.3 first run (D4.4). `/` lands here when the account has nothing yet. */
export const Route = createFileRoute('/_authed/welcome')({
  component: WelcomePage,
});
