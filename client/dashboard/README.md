# BackendRelay dashboard

The self-hosted BackendRelay dashboard: TanStack Start in **SPA mode** (no SSR, no Node
server at runtime), React 19, TypeScript 6 and Vite 8. Caddy serves the resulting static
files in production (see `deploy/`, added in D1.4 of the plan).

Pinned versions (2026-10-25 research, `research/dashboard-tech-stack.md` in the plan):

| Package                         | Pin             |
| ------------------------------- | --------------- |
| react / react-dom               | 19.3.1          |
| @types/react / @types/react-dom | 19.3.1 / 19.3.0 |
| typescript                      | 6.0.3           |
| vite / @vitejs/plugin-react     | 8.3.5 / 6.1.2   |
| @tanstack/react-start           | 1.169.4         |
| @tanstack/react-router          | 1.170.46        |
| @tanstack/react-query           | 5.104.5         |
| zod                             | 4.6.7           |

Node `>=22.12.0` is required for builds; CI builds on Node 24 LTS.

Install dependencies from `client/` with `pnpm install` (this package's pins change
`client/pnpm-lock.yaml`).

## Scripts

| Script      | Command                                     | Notes                                                                   |
| ----------- | ------------------------------------------- | ----------------------------------------------------------------------- |
| `dev`       | `vite dev --port 3100 --strictPort`         | Dev server on <http://localhost:3100>.                                  |
| `dev:mock`  | `VITE_MOCK_API=1 vite dev …`                | Dev server with the MSW handlers under `src/test/msw/`.                 |
| `build`     | `vite build`                                | Static output only, see below.                                          |
| `build:e2e` | `VITE_MOCK_API=1 VITE_CATALOG=1 vite build` | Mock-mode build with the component catalog, used by Playwright.         |
| `preview`   | `vite preview --port 4173 --strictPort`     | Serves the build on <http://127.0.0.1:4173>; Playwright uses this port. |
| `typecheck` | `tsc --noEmit`                              | `strict`, `noUncheckedIndexedAccess`, `verbatimModuleSyntax`.           |
| `lint`      | `eslint . --max-warnings 0`                 | TypeScript strict type-checked, React, a11y and TanStack rules.         |
| `test:ci`   | `vitest run`                                | Vitest 5 + jsdom + Testing Library + MSW.                               |
| `e2e`       | `playwright test`                           | Chromium light/dark plus Firefox and WebKit smoke, with axe.            |
| `size`      | `size-limit`                                | Enforces the budgets in `.size-limit.json` after a build.               |

Run them from `client/` as `pnpm --filter backendrelay-dashboard <script>`.

## Testing

- **Unit tests:** `vitest run` in jsdom, with Testing Library and `jest-dom`.
  `src/test/setup.ts` starts a shared MSW server, and `src/test/render.tsx` wraps the
  real router in the Query and Router providers.
- **Mocks:** `src/test/msw/handlers.ts` is the single set of MSW 3 handlers. Vitest uses
  them through `src/test/msw/node.ts`; the dev server and the e2e build use them through
  `msw/vite` when `VITE_MOCK_API=1` is set (`pnpm dev:mock`, `pnpm build:e2e`).
- **Browser tests:** `pnpm e2e` runs `build:e2e` and `preview` on port 4173, then
  Chromium in light and dark plus Firefox and WebKit smoke projects. `e2e/fixtures.ts`
  exposes `checkA11y` (axe, WCAG 2.2 AA tags). Install the browsers with
  `pnpm exec playwright install chromium firefox webkit` once per machine, or use the
  Playwright container.
- **Live journey:** `client/dashboard/e2e/live/` is the `live` project (D9.2). It runs
  only with `BR_LIVE=1` and expects `BR_LIVE_DASHBOARD_URL` plus
  `BR_LIVE_SERVER_LOG`; `make verify-dashboard-browser` from the repository root
  provisions the server, database, RustFS bucket and preview for it.
- **Component catalog:** `/_dev/catalog` renders every design-system component and state.
  It is available from `pnpm dev` and from builds made with `VITE_CATALOG=1`; the
  Playwright build enables it. Visual baselines are taken only in the Playwright
  container: `PLAYWRIGHT_VISUAL=1 pnpm e2e --grep @visual --update-snapshots`.
- **Budgets:** after `pnpm build`, `pnpm size` checks the `.size-limit.json` entries
  (entry ≤ 250 KB gzip, explorer chunks ≤ 420 KB, editor chunk ≤ 120 KB, CSS ≤ 40 KB).
- **Performance:** `e2e/performance.spec.ts` seeds 10k mock rows and records a trace while
  scrolling the Explorer; a Long Tasks observer fails the test on any task over 200ms.
  `e2e/lazy-routes.spec.ts` visits every app section and asserts its own route chunk loads.

## Build output (input for D1.4)

Recorded for the container work in D1.4:

- output directory: `client/dashboard/dist/client/`
- HTML shell: `client/dashboard/dist/client/_shell.html`

TanStack Start's SPA mode prerenders that single shell and emits the hashed JS/CSS under
`dist/client/assets/`; `dist/server/` is build-time-only and is never served. Caddy must
rewrite unknown paths to `/_shell.html` (`try_files {path} /_shell.html`), **not** to
`index.html`.

> Status: the stack was pinned and the routes scaffolded by an executor that could not run
> `pnpm` (the sandbox denies it). D1.4 hard-codes the two paths above from the plan.
> Confirm them with `pnpm --filter backendrelay-dashboard build` and check that nothing in
> `dist/` requires a Node server. If SPA mode turns out to need one at runtime, stop and
> fall back to plain Vite + TanStack Router (needs owner approval). The dashboard's
> dependencies also need one `pnpm install` from `client/` so its lockfile importer exists
> before `--frozen-lockfile` installs work; `client/pnpm-lock.yaml` has no `dashboard`
> importer yet, so the container build fails at `pnpm install` until it does.

## Container

`deploy/Dockerfile` builds the static output on `node:24.22.1-slim` and copies
`dist/client/` into a `caddy:2.11.7-alpine` runtime image that runs as UID 10001 and
listens on port 8080. The build context is `client/`:

```sh
docker build -f client/dashboard/deploy/Dockerfile client
docker run --rm -p 8080:8080 -e BR_API_URL=http://localhost:8888 backendrelay-dashboard
```

`deploy/Caddyfile` serves `/config.json` from environment variables at container start,
so changing the API URL only needs a restart:

- `BR_API_URL` (required): API origin, served as `apiBaseUrl`.
- `BR_DASHBOARD_API_URL`: gateway origin for `/dash`, `/superadmin` and
  `/platform/oauth`.
- `BR_STORAGE_ORIGIN`: public storage origin allowed for file previews.
- `BR_WS_ORIGIN`: realtime WebSocket origin. Set it, or the CSP blocks live data.

At runtime `src/lib/runtime-config.ts` fetches `/config.json`, validates it with zod and
shows a plain error page instead of the app when it is missing or invalid. In development
Vite serves `public/config.json`, which points at `http://localhost:8888`.

Other container details: `/assets/*` is cached immutably (hashed filenames), the HTML
shell is `no-cache`, unknown paths fall back to `/_shell.html`, `/_devtool*` alone may be
framed, and `GET /healthz` returns `200 ok` for compose healthchecks.

## Folder structure

```
client/dashboard/
  package.json  tsconfig.json  vite.config.ts  eslint.config.js
  vitest.config.ts  playwright.config.ts  .size-limit.json
  components.json                                  # D2.2
  public/{config.json,theme-init.js}                 # D1.4/D2.1
  deploy/{Caddyfile,Dockerfile}                      # D1.4
  e2e/{fixtures.ts,index.spec.ts,catalog.spec.ts,performance.spec.ts,lazy-routes.spec.ts,__screenshots__/}
  src/
    router.tsx  routeTree.gen.ts (generated)  styles/globals.css
    routes/  __root.tsx [_]dev.catalog.tsx  login.tsx _authed.tsx
      _authed/ orgs/$orgId/members.tsx  settings/{tokens,oauth-apps}.tsx  operator/…
      _authed/apps/ index.tsx $appId.tsx
      _authed/apps/$appId/ explorer.tsx schema.tsx permissions.tsx users.tsx storage.tsx
                           auth/{providers,email-templates}.tsx webhooks/{index,$webhookId}.tsx
                           backups.tsx sandbox.tsx usage.tsx
    features/catalog/catalog-page.tsx                 # /_dev/catalog (dev only)
    features/<area>/{api.ts,queries.ts,schemas.ts,components/,*.test.tsx}
    components/{ui/(shadcn only),layout/,data/,data-grid/,code-editor/,charts/,feedback/}
    lib/{api/{client.ts,endpoints/},runtime-config.ts,auth/,instant.ts,clipboard.ts,format/}
    test/{setup.ts,render.tsx,msw/,fixtures/api/}
```

Routes live in `src/routes/`; colocated non-route files are prefixed with `-`.
`src/routeTree.gen.ts` is generated by the TanStack Router plugin during `dev` and
`build` — never edit it by hand (it is excluded from lint and format).
