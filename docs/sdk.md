# Client SDK

Apps talk to BackendRelay through the client SDKs published under the
`@backendrelay` npm scope. They are a fork of InstantDB's `@instantdb/*` SDKs
with the hosted-cloud defaults removed: an app must point them at your server,
there is no fallback to `instantdb.com`.

This guide covers installing the packages, the configuration every app must
set, how a new SDK version is released and rolled out, and what the server
promises to existing versions. For the publish pipeline itself (npm scope,
provenance, tags, Verdaccio), see [Publishing the SDK](sdk-publishing.md).

## Contents

- [Install](#install)
- [Required configuration](#required-configuration)
  - [`apiURI`](#apiuri)
  - [`websocketURI`](#websocketuri)
  - [Devtool](#devtool)
  - [Admin token](#admin-token)
  - [CLI](#cli)
- [Supported runtimes](#supported-runtimes)
- [Tested SDK version](#tested-sdk-version)
- [Upgrade procedure](#upgrade-procedure)
- [Compatibility promise](#compatibility-promise)

## Install

Install the package for your framework:

| App | Package |
| --- | --- |
| Plain JavaScript, or another framework binding | `@backendrelay/core` |
| React | `@backendrelay/react` |
| React Native | `@backendrelay/react-native` |
| React Native with MMKV storage | `@backendrelay/react-native-mmkv` |
| Expo with SQLite storage | `@backendrelay/expo-sqlite` |
| Server-side scripts and jobs | `@backendrelay/admin` |
| Schema, permission and dashboard tooling | `@backendrelay/cli` |
| Platform management API | `@backendrelay/platform` |
| Prebuilt React UI | `@backendrelay/components` |

```sh
pnpm add @backendrelay/react
# or
npm install @backendrelay/react
```

The full package list, including `@backendrelay/webhooks`,
`@backendrelay/resumable-stream` and `@backendrelay/version`, is in
[Publishing the SDK](sdk-publishing.md#publishing-the-client-sdk).

All packages share one version and must be upgraded together. Pin the exact
version in `package.json` (`1.1.0`, not `^1.1.0`) so a canary rollout is a real
decision rather than whichever release npm served that day.

The CLI is normally a development dependency; `@backendrelay/cli` installs the
`instant-cli` binary.

## Required configuration

### `apiURI`

Every client and the Admin SDK require `apiURI`, the public origin of your
BackendRelay server. Without it they throw `MissingApiURIError` at `init`
instead of sending data to the hosted service. The value must be an absolute
`http://` or `https://` URL; trailing slashes are stripped.

```ts
import { init } from '@backendrelay/react';

const db = init({
  appId: process.env.NEXT_PUBLIC_INSTANT_APP_ID!,
  apiURI: 'https://api.example.com',
});
```

The Admin SDK takes the same option and must only run where its admin token is
safe:

```ts
import { init } from '@backendrelay/admin';

const db = init({
  appId: process.env.INSTANT_APP_ID!,
  adminToken: process.env.INSTANT_APP_ADMIN_TOKEN!,
  apiURI: process.env.INSTANT_API_URI!,
});
```

Keep one origin per environment. Browser builds need a public value; server
components and workers can read it from their own environment. See
[Configuration](configuration.md) for the server side (`INSTANT_BACKEND_URL`
and friends) and [Getting started](getting-started.md) for a local run.

### `websocketURI`

`websocketURI` is derived from `apiURI` unless you pass it explicitly:
`https://api.example.com` becomes `wss://api.example.com/runtime/session`, and
`http://localhost:8888` becomes `ws://localhost:8888/runtime/session`. The
derived value is correct when the realtime endpoint is served by the same
origin as the HTTP API, which is the normal deployment.

Set `websocketURI` only when realtime traffic goes somewhere else, for example
through a separate proxy host, or when a proxy rewrites the path. A wrong
`websocketURI` shows up as a connection that never reaches `ready`; the SDK then
retries with jittered backoff and can fall back to SSE on `/runtime/sse`.

### Devtool

The devtool is opt-in. Pass a `devtool` object with a `dashURI` pointing at
your dashboard origin; without one, mounting the devtool throws instead of
guessing a hosted URL:

```ts
const db = init({
  appId,
  apiURI: 'https://api.example.com',
  devtool: { dashURI: 'https://dash.example.com' },
});
```

It only mounts in a browser whose hostname is in `devtool.allowedHosts`
(default `localhost`), so a production build that forgets to remove the option
still shows nothing to users on your public domain.

### Admin token

The dashboard hands out an app's `adminToken`. It bypasses permission rules,
so it belongs in server-side environment variables only — never in a browser
bundle, a mobile app, or a repository. Rotating it in the dashboard replaces the
stored token, so the old value stops working immediately.

### CLI

The CLI reads `instant.config.ts` (or `.js`/`.json`) for `apiURI` and
`dashURI`. In CI, prefer environment variables:

| Variable | Purpose |
| --- | --- |
| `INSTANT_CLI_API_URI` | Management API origin |
| `INSTANT_CLI_DASHBOARD_API_URI` | Dashboard API origin, when it differs |
| `INSTANT_CLI_DASH_URI` | Dashboard origin for browser hand-off |

```ts
// instant.config.ts
export default {
  apiURI: 'https://api.example.com',
  dashURI: 'https://dash.example.com',
};
```

Set one of these explicitly in every environment. The CLI falls back to its
upstream default origin when none is configured, so a missing setting silently
points at the wrong service rather than failing.

## Supported runtimes

The client and Admin SDKs only assume standard web APIs: `fetch`,
`Headers`/`Request`/`Response`, Web Streams, `crypto.getRandomValues`, and
`WebSocket` for realtime. The Admin SDK has no runtime Node imports; its
storage helper detects a Node `Readable` structurally instead of importing
`node:stream`, so its published types do not need `@types/node`.

| Runtime | Client SDKs | Admin SDK | Covered by |
| --- | --- | --- | --- |
| Node.js 22 and 24 | yes | yes | Admin compatibility lane and the SDK e2e suites |
| Chromium (browser build) | yes | not for production | Browser compatibility lane |
| Bun 1.x | yes | yes | One-off runtime harness (guest query, admin query and transact) |
| Cloudflare Workers (`wrangler dev`, no `nodejs_compat`) | yes | yes for HTTP calls | One-off runtime harness (guest query, admin query and transact) |

- **Server rendering.** A framework server component or a Worker can fetch a
  query's triples directly instead of opening a WebSocket. This is the same
  request the SDK makes on the server ([`getTriplesAndAttrsForQuery`](../client/packages/core/src/framework.ts)):

  ```ts
  const res = await fetch(`${apiURI}/runtime/framework/query`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      'app-id': appId,
      // Only for an authenticated user; omit it for a guest query:
      // authorization: `Bearer ${refreshToken}`,
    },
    body: JSON.stringify({ query: { todos: {} } }),
  });
  if (!res.ok) throw new Error(`framework query failed: ${res.status}`);
  const { result, attrs } = await res.json();
  ```

  Rules apply exactly as they do for a browser client: a request without an
  `authorization` header is a guest, so the queried namespace must allow guest
  reads. The response carries the raw datalog result and the attr store; cache
  it per request, not across requests.

- **Realtime.** `subscribeQuery`/`useQuery` need WebSocket or SSE support and
  work in Bun and in Workers, but the runtime harness only checks the HTTP
  paths. Streaming (`db.streams`) additionally needs `ReadableStream`/
  `WritableStream` globals.
- **Storage.** `uploadFile` accepts a `Uint8Array` (Node's `Buffer` is one), a
  web `ReadableStream`, or a Node `Readable`. Workers have no `Buffer`; pass the
  bytes as a `Uint8Array` from `Response.arrayBuffer()` or an incoming body.
- **Not covered.** React Native runtimes, Deno, and Vercel Edge are not in the
  harness. They use the same web APIs and usually work, but a runtime-specific
  failure there is not a release blocker until the runtime is added above.

## Tested SDK version

`MAX_TESTED_SDK_VERSION` in
[`instant-realtime/src/protocol.rs`](../server-rs/crates/instant-realtime/src/protocol.rs)
names the newest published SDK release this server has been exercised against.
It is a `(major, minor, patch)` tuple and is compared against the core client
version each session reports.

- At or below the constant: supported, and covered by the compatibility lanes
  described below.
- Above it: still served. The server negotiates wire features from the version
  it is given, so a newer client does not break, but nothing is promised about
  features it introduced after the constant.

Release rule: bump the constant in the same pull request that bumps
`client/packages/version/src/version.ts`. A unit test in `protocol.rs` fails if
the constant no longer covers every negotiated feature threshold, so it cannot
be lowered below the feature set the server already relies on. Server release
notes should state the constant's new value.

## Upgrade procedure

This is the sequence for taking a new upstream InstantDB SDK release into
BackendRelay and rolling it out. A release that only fixes a bug inside an
existing line skips the port step but still verifies, publishes and rolls out
the same way.

### 1. Detect

- Check the nightly `upstream-watch` job summary; it records
  `npm view @instantdb/core dist-tags`.
- Read upstream's `client/packages` history, for example
  `git log c33e6213..upstream/main -- client/packages`, and the upstream release
  notes.
- Watch the SDK versions reported in production
  (`backendrelay_sdk_sessions{sdk,version}`) for clients that are ahead of
  `MAX_TESTED_SDK_VERSION`; those are the users most likely to hit a gap.

### 2. Port

1. Add upstream as a fetch-only remote if it is not configured yet, then
   inspect the delta. Never push to it.
2. Classify every change:
   - **Client-only** (state handling, types, React hooks): port it.
   - **Wire-level** (a new operation, a new connection header, a new admin or
     platform route): port it and add the matching Rust server support with
     tests in the same pull request. A client that speaks a new operation to a
     server that ignores it is worse than not porting the change.
   - **Server-only** (upstream Clojure): reimplement it in Rust only when the
     behaviour is visible to clients.
3. Keep the workspace's internal `@instantdb/*` names. Renaming is a publish
   concern that `client/scripts/publish_packages.mjs` handles.
4. Do not reintroduce hosted defaults. Search before finishing:
   `git grep -n 'instantdb\.com' -- client/packages` must only match comments
   that explain the fork.

### 3. Verify

From `client/`:

```sh
pnpm install --frozen-lockfile
pnpm build-packages
pnpm test:local
pnpm check-format
```

From the repository root:

```sh
make verify-quick
make verify-local
```

Then the SDK acceptance suites, which exercise the real client against a real
server (they need Docker, and a Cloudflare R2 configuration for the native
harness):

```sh
bash self-hosting/run-official-sdk-e2e-native.sh --suite full
bash self-hosting/run-official-sdk-e2e.sh --published-version <version>
```

If the port changed anything on the wire, also run the PostgreSQL acceptance
tests for the affected crates (see [Development](development.md#browser-and-sdk-acceptance)).
Before the first publish of a new line, rehearse the tarballs against
Verdaccio as described in [Publishing the SDK](sdk-publishing.md#rehearsal-against-verdaccio).

### 4. Publish

1. Bump `client/packages/version/src/version.ts`.
2. Bump `MAX_TESTED_SDK_VERSION` in `protocol.rs` to the same version.
3. Merge, then let the [Publish SDK](../.github/workflows/publish-sdk.yml)
   workflow publish on the `sdk-v<version>` tag, or dispatch it with `dry-run`
   first.

Publishing changes what new installs receive; it does not upgrade running
apps. That is the rollout below.

### 5. Roll out

The server serves clients newer than `MAX_TESTED_SDK_VERSION`, so an app can
take the new SDK before the server release that bumps the constant ships. Either
order works; upgrade in stages:

1. **Canary apps.** The dashboard (`client/www`) builds against the workspace
   packages and is the cheapest signal. If you run the realtime sentinel app,
   upgrade it here too. Then one low-traffic production app.
2. **Observe.** For at least a day, watch
   `backendrelay_sdk_sessions{sdk,version}` for the new version appearing,
   alongside realtime connection errors, refresh failures and error logs. A
   spike that starts with the canary pins the release.
3. **The rest.** Upgrade the remaining apps in dependency order (core first,
   framework packages after). Every package in an app moves together.
4. **Keep the floor honest.** Once no live session reports an old version,
   update `SDK_COMPAT_OLDEST_VERSION` so the nightly compatibility lane follows
   production.

If the release misbehaves, deprecate it, roll the canary pins back and publish
a fix. See [Rollback](sdk-publishing.md#rollback); do not unpublish a version
that apps still pin.

## Compatibility promise

- **Older clients keep working.** A server release serves every published SDK
  version at or below `MAX_TESTED_SDK_VERSION`. Wire features are negotiated
  from the version the client reports, so an old client gets the full-payload
  path rather than an error.
- **The floor follows production.** The nightly `sdk-compat` lane runs the Core
  and Admin end-to-end suites against the oldest version still reported in
  production, not only the newest. A version is not dropped while apps still
  report it.
- **Deprecation is explicit.** Removing support for an SDK version is called
  out in the server release notes and leaves at least one minor server release
  in between, so operators have a release cycle to pin a replacement.
- **Newer clients are accepted, not promised.** Versions above
  `MAX_TESTED_SDK_VERSION` connect and are served, but their new features are
  only claimed once the constant is bumped and the release is exercised by the
  compatibility lanes.
- **Additive wire changes.** New operations and fields are opt-in by version.
  Breaking wire changes ship as a new major SDK line with a server release that
  supports both sides during the transition.
- **Tested runtimes.** The compatibility lanes run in Chromium and Node (see
  [Development](development.md)). The runtime harness behind
  [Supported runtimes](#supported-runtimes) adds Bun and Cloudflare Workers.
  Runtime-specific failures there are release blockers; runtimes outside
  those are not covered until they are added.
