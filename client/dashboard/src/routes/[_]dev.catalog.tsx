import {
  createFileRoute,
  lazyRouteComponent,
  notFound,
} from '@tanstack/react-router';

/**
 * The component catalog is a development tool. It is reachable from the dev
 * server and from builds that opt in with `VITE_CATALOG=1`; every other build
 * strips the catalog chunk because `catalogEnabled` is a build-time constant.
 *
 * The file name escapes the leading underscore (`[_]dev`) so the URL is
 * `/_dev/catalog`; an unescaped `_dev` would be a pathless layout.
 */
const catalogEnabled =
  import.meta.env.DEV || import.meta.env.VITE_CATALOG === '1';

export const Route = createFileRoute('/_dev/catalog')({
  beforeLoad: () => {
    if (!catalogEnabled) {
      throw notFound();
    }
  },
  head: () => ({ meta: [{ title: 'Component catalog · BackendRelay' }] }),
  component: catalogEnabled
    ? lazyRouteComponent(
        () => import('@/features/catalog/catalog-page'),
        'CatalogPage',
      )
    : CatalogDisabled,
});

function CatalogDisabled() {
  return null;
}
