# Publishing the client SDK

The BackendRelay SDK is published to npm under the `@backendrelay` scope:

| Package | Directory |
|---|---|
| `@backendrelay/core` | `client/packages/core` |
| `@backendrelay/react` | `client/packages/react` |
| `@backendrelay/react-common` | `client/packages/react-common` |
| `@backendrelay/react-native` | `client/packages/react-native` |
| `@backendrelay/react-native-mmkv` | `client/packages/react-native-mmkv` |
| `@backendrelay/expo-sqlite` | `client/packages/expo-sqlite` |
| `@backendrelay/admin` | `client/packages/admin` |
| `@backendrelay/platform` | `client/packages/platform` |
| `@backendrelay/components` | `client/packages/components` |
| `@backendrelay/webhooks` | `client/packages/webhooks` |
| `@backendrelay/resumable-stream` | `client/packages/resumable-stream` |
| `@backendrelay/version` | `client/packages/version` |
| `@backendrelay/cli` | `client/packages/cli` (installs the `instant-cli` binary) |

The workspace itself keeps upstream's `@instantdb/*` names, because the pnpm
lockfile, the turbo filters and the acceptance suites all refer to them. The
publish script stages a copy instead of renaming in place:

- the staged `package.json` takes the `@backendrelay` name, the shared version,
  and `^<version>` for sibling packages instead of `workspace:*`;
- sibling imports in `dist/` and the shipped sources move to the new scope.
  Third-party packages that happen to share upstream's scope, such as
  `@instantdb/eventsource`, keep their names;
- `homepage` and `repository` point at this repository.

Nothing in the working tree changes, so a failed run never leaves
half-versioned `package.json` files behind. The package list lives in
`client/scripts/publish_packages.mjs` (order matters: dependencies first), and
`client/scripts/publish-packages.test.mjs` fails if the list and
`client/packages/` disagree.

## Versioning

All packages share one version, `client/packages/version/src/version.ts`
(currently `v1.1.0`). The published line started at `1.1.0` rather than
continuing upstream's `1.0.x`.

The server's compatibility claim follows the release: bump
`MAX_TESTED_SDK_VERSION` in `instant-realtime/src/protocol.rs` in the same pull
request as `version.ts`. `docs/sdk.md` covers the policy, the rollout and the
compatibility promise.

Prereleases carry a suffix that matches the dist-tag, for example
`v1.1.1-experimental.0` published with `DIST_TAG=experimental`. The script
refuses a prerelease on `latest` and refuses a dist-tag that the version does
not contain.

## Owner setup, once

1. Create the npm organisation `backendrelay` and make it the owner of the
   scope. Every package in the table above must be published by a member of
   that organisation.
2. Enable two-factor authentication on the publishing account, then create a
   **granular access token** with read/write access to the `@backendrelay`
   scope (automation token: bypasses 2FA prompts when publishing).
3. Store it as the repository secret `NPM_TOKEN`. The
   [Publish SDK](../.github/workflows/publish-sdk.yml) workflow uses it.
4. Do the first publish by hand (below). After that, publish from CI only, so
   provenance and the token stay off developer machines.

## Rehearsal against Verdaccio

Dry-run first: it packs every package and prints the tarball contents without
contacting a registry.

```sh
cd client
make publish-dry-run
```

Then rehearse a real publish against a throwaway registry:

```sh
mkdir -p /tmp/verdaccio && cd /tmp/verdaccio
npx verdaccio &                                # serves http://localhost:4873
npm adduser --registry http://localhost:4873   # any credentials; local only

cd <repository>/client
pnpm build-packages
node ./scripts/publish_packages.mjs --registry http://localhost:4873
npm view @backendrelay/core version --registry http://localhost:4873
```

Install one of the published packages into a scratch app from that registry
(`npm install @backendrelay/core --registry http://localhost:4873`) and confirm
it resolves its siblings. Stop Verdaccio and delete `/tmp/verdaccio`; the
storage directory holds the tarballs.

## Publishing

From `client/` with npm credentials for the scope:

```sh
make publish                          # the release version, dist-tag latest
DIST_TAG=experimental make publish    # a prerelease channel
```

`make publish` builds every package first (`pnpm build-packages`), then hands
each one to `npm publish` from its staged copy. The script enables
`--provenance` when `GITHUB_ACTIONS=true`, and refuses packages with no `dist/`
so an unbuilt tree cannot be published.

If a run stops halfway, it prints the packages that did make it. Resume with the
rest:

```sh
node ./scripts/publish_packages.mjs --package core --package react
```

A version can be published only once. To ship a correction, bump
`version.ts`, merge, and publish again.

## From CI

`.github/workflows/publish-sdk.yml` publishes on `sdk-v*` tags. The tag must
equal `sdk-v` plus the version in `version.ts`, so `sdk-v1.1.0` publishes
`1.1.0`; the job fails before uploading on a mismatch. It also runs on demand
with `dry-run` enabled by default.

Publishing with provenance needs a public repository or an npm plan that
supports provenance for private ones. If npm rejects the attestation, set
`PUBLISH_PROVENANCE=0` in the workflow to publish without it and record why.

## Compatibility lanes

Nightly runs `sdk-compat` (`nightly.yml`). It resolves the exact scope and
version pinned by `examples/*/package.json` and runs the Core and Admin e2e
suites against the published npm tarballs of that version, not the workspace
`dist/`. The same lane runs against the oldest version still reported in
production by `backendrelay_sdk_sessions`; the CI runner holds no production
credentials, so an operator reads it where Prometheus is reachable:

```sh
PROMETHEUS_URL=https://prometheus.example node self-hosting/sdk-compat/oldest-sdk-version.mjs
```

Put the result in the repository variable `SDK_COMPAT_OLDEST_VERSION`. Set
`SDK_COMPAT_OLDEST_SCOPE` when the oldest version is an upstream `@instantdb`
release that predates the `@backendrelay` line. A manual nightly dispatch can
pass `oldest-version` instead.

To rehearse the lane before publishing to npm, use the Verdaccio rehearsal
above and point the harness at the tarballs it stored:

```sh
bash self-hosting/run-official-sdk-e2e.sh \
  --published-version 1.1.0 \
  --core-tarball <verdaccio-storage>/backendrelay-core-1.1.0.tgz \
  --admin-tarball <verdaccio-storage>/backendrelay-admin-1.1.0.tgz
```

## Rollback

npm allows `npm unpublish <name>@<version>` only within 72 hours and only if no
other package depends on it. After that, deprecate instead, once per affected
package:

```sh
npm deprecate @backendrelay/core@1.1.0 'Broken release; use 1.1.1'
```

Then publish a fixed version. Yanking a version that apps already pin is the
last resort, not the first.
