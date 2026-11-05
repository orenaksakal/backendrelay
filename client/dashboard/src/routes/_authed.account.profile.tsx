import { createFileRoute } from '@tanstack/react-router';
import { ProfilePage } from '@/features/account/profile-page';

/** C.18 Profile. */
export const Route = createFileRoute('/_authed/account/profile')({
  component: ProfilePage,
});
