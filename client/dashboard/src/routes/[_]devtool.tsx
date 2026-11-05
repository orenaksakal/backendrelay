import { createFileRoute } from '@tanstack/react-router';
import { DevtoolPage } from '@/features/devtool/devtool-page';

export interface DevtoolSearch {
  /** The SDK opens `/_devtool?appId=<id>` (C.20 contract path). */
  appId?: string;
}

/**
 * C.20 SDK devtool. The file name escapes the leading underscore (`[_]devtool`)
 * so the URL is `/_devtool`; an unescaped `_devtool` would be a pathless
 * layout. The route sits outside `_authed` because it signs in inline.
 */
export const Route = createFileRoute('/_devtool')({
  validateSearch: (search: Record<string, unknown>): DevtoolSearch => ({
    appId:
      typeof search['appId'] === 'string' && search['appId'] !== ''
        ? search['appId']
        : undefined,
  }),
  head: () => ({ meta: [{ title: 'Devtool · BackendRelay' }] }),
  component: DevtoolRoute,
});

function DevtoolRoute() {
  const { appId } = Route.useSearch();
  return <DevtoolPage appId={appId} />;
}
