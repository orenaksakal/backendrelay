import { Kbd, KbdGroup } from '@/components/ui/kbd';
import type { App } from '@/lib/api/endpoints/schemas';

/**
 * C.20 Help tab: the compact stand-in for the old devtool's help card. The SDK
 * toggles the iframe with Ctrl+Shift+0 and closes it on Esc, so the links point
 * at the full dashboard where the larger screens live.
 */
export function DevtoolHelp({ app }: { app: App }) {
  return (
    <div className="flex flex-col gap-4 overflow-auto p-3 text-sm">
      <div>
        <h2 className="text-section">BackendRelay devtool</h2>
        <p className="text-muted-foreground mt-1 text-xs">
          This widget embeds a data explorer and a query playground for your
          app. It only appears in development builds, so you can inspect data
          without leaving your site.
        </p>
      </div>

      <div>
        <h3 className="text-sm font-medium">Keyboard</h3>
        <ul className="text-muted-foreground mt-1 flex flex-col gap-1 text-xs">
          <li>
            <KbdGroup>
              <Kbd>Ctrl</Kbd>
              <span aria-hidden="true">+</span>
              <Kbd>Shift</Kbd>
              <span aria-hidden="true">+</span>
              <Kbd>0</Kbd>
            </KbdGroup>{' '}
            toggles the devtool.
          </li>
          <li>
            <Kbd>Esc</Kbd> closes it.
          </li>
        </ul>
      </div>

      <div>
        <h3 className="text-sm font-medium">Open in the dashboard</h3>
        <ul className="mt-1 flex flex-col gap-1 text-xs">
          <li>
            <a
              className="text-accent inline-flex min-h-6 items-center hover:underline"
              href={`/apps/${app.id}/overview`}
              target="_blank"
              rel="noreferrer"
            >
              {`${app.title} overview`}
            </a>
          </li>
          <li>
            <a
              className="text-accent inline-flex min-h-6 items-center hover:underline"
              href={`/apps/${app.id}/explorer`}
              target="_blank"
              rel="noreferrer"
            >
              Explorer
            </a>
          </li>
          <li>
            <a
              className="text-accent inline-flex min-h-6 items-center hover:underline"
              href={`/apps/${app.id}/settings`}
              target="_blank"
              rel="noreferrer"
            >
              App settings
            </a>
          </li>
        </ul>
      </div>

      <p className="text-muted-foreground text-xs">
        The devtool needs the admin token for this app. Sign in with an account
        that can administer it, or open the full dashboard to manage access.
      </p>
    </div>
  );
}
