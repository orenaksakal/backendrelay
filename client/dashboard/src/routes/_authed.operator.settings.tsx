import { createFileRoute } from '@tanstack/react-router';
import { OperatorSettingsPage } from '@/features/operator/operator-settings-page';

/** C.19 Operator settings. */
export const Route = createFileRoute('/_authed/operator/settings')({
  component: OperatorSettingsPage,
});
