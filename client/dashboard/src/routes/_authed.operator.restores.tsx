import { createFileRoute } from '@tanstack/react-router';
import { OperatorRestoresPage } from '@/features/operator/operator-restore-jobs-page';

/** C.19 Restore jobs. */
export const Route = createFileRoute('/_authed/operator/restores')({
  component: OperatorRestoresPage,
});
