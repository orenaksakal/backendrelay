# Dashboard HTTP API

This page documents the dashboard control-plane endpoints served by `instant-server` under `/dash`. The routes are implemented in [`instant-http`](../server-rs/crates/instant-http/src/dashboard.rs) and dispatched by the dashboard services in [`instant-server`](../server-rs/crates/instant-server/src).

## `GET /dash/config`

Deployment metadata the dashboard needs before a user signs in. This endpoint is unauthenticated and never returns user records, app IDs, emails or secret values.

```json
{
  "version": "dev",
  "googleLogin": false,
  "signup": "closed",
  "ephemeralApps": false,
  "private": false
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `version` | string | The build version stamped at compile time with `INSTANT_BUILD_VERSION`, or `dev` for a local build. |
| `googleLogin` | boolean | Whether a dashboard Google OAuth client is configured. |
| `signup` | `open` \| `restricted` \| `closed` | Dashboard signup policy. `restricted` means an allowlist applies; only listed emails, plus the superuser, may create accounts. |
| `ephemeralApps` | boolean | Whether unauthenticated ephemeral app creation is enabled. |
| `private` | boolean | Whether the server runs in private dashboard gateway mode. |

The response always carries `Cache-Control: no-store`.

### Sources

- `version`: build-time `INSTANT_BUILD_VERSION`; `dev` when unset.
- `googleLogin`: `INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_ID` and `INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_SECRET`.
- `signup`: `INSTANT_DASHBOARD_ALLOWED_EMAILS` makes signup `restricted`. Otherwise the `dashboard-signups` config-app flag supplies `open`, `restricted` or `closed`; unset falls back to `INSTANT_DASHBOARD_SIGNUP` and then `closed`.
- `ephemeralApps`: the `ephemeral-apps-enabled` config-app flag, falling back to `INSTANT_EPHEMERAL_APPS`.
- `private`: `INSTANT_DASHBOARD_PRIVATE`.

In private mode the dashboard gateway still requires its origin credential on `/dash/*`, including this endpoint. "Unauthenticated" means no dashboard user session is required.

## `GET /dash/apps/:app_id/usage`

Stored bytes, file storage and write volume for one app. Requires the app member role **collaborator** or higher. Dashboard sessions, personal access tokens and OAuth management tokens with `apps-read` are accepted, and an app admin token for the same app counts as owner.

```json
{
  "appBytes": 4200000,
  "storageBytes": 125829120,
  "fileCount": 120,
  "liveSessions": 0,
  "transactions": {
    "last24h": 12,
    "last7d": 210,
    "daily": [
      { "date": "2026-10-13", "count": 31 },
      { "date": "2026-10-14", "count": 0 }
    ]
  }
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `appBytes` | integer | Sum of the per-attribute PostgreSQL sizes in `triples_size_aggregate`. `0` when the app has no collected sizes yet. |
| `storageBytes` | integer | Sum of the recorded `$files.size` bytes. `0` when no file has been uploaded. |
| `fileCount` | integer \| null | Number of textual `$files` rows from `attr_sketches`, or `null` when the sketch has not been built. |
| `liveSessions` | integer | Cluster-wide live realtime sessions for the app. `0` when no machine reports sessions. |
| `transactions.last24h` | integer | Raw `transactions` rows created in the last 24 hours. |
| `transactions.last7d` | integer | Raw `transactions` rows created in the last 7 days. |
| `transactions.daily` | array | 14 entries, oldest first, for `current_date - 13` through `current_date`. |

`daily` comes from `daily_app_transactions` and is filled with zeroes for days without a row. The scheduled daily metrics pass writes those rows, so the newest entry can lag behind `last24h`.

## `GET /dash/apps-summary?workspace=<personal|orgId>`

Usage summaries for every app the caller can see in one workspace. Requires a dashboard session token.

```json
{
  "apps": [
    {
      "id": "3f2a1b4c-5d6e-4f70-8192-a3b4c5d6e7f8",
      "liveSessions": 12,
      "appBytes": 4200000,
      "storageBytes": 125829120,
      "lastWriteAt": "2026-10-26T18:04:11Z",
      "lastBackupAt": "2026-10-26T06:00:00Z"
    }
  ]
}
```

- `workspace=personal` covers personally owned apps plus apps where the caller is an app member. Apps marked for deletion are excluded.
- `workspace=<orgId>` covers the org's apps when the caller is an org member, plus individual apps where the caller is an app member. A caller with no access to the org gets `401`.
- A missing or malformed `workspace` returns `400`.
- `lastWriteAt` is `null` when the app has no transaction, and `lastBackupAt` is `null` when the app has no unexpired backup that is not marked for deletion.
- `liveSessions` is the cross-machine in-memory session total. Apps with no live sessions report `0`.

The endpoint issues one app-visibility query plus one grouped query per metric over the workspace's app IDs. It never loops per app.

## `POST /dash/apps/:app_id/rules/validate`

Compiles a rules document and reports its errors without saving it. Requires the app member role **collaborator** or higher. Dashboard sessions, personal access tokens and OAuth management tokens with `apps-read` are accepted, and an app admin token for the same app counts as owner.

```json
{ "code": { "goals": { "allow": { "view": "true" } } } }
```

A document that would save successfully returns:

```json
{ "ok": true }
```

Compilation uses the same `RuleSet::try_new` validation as `POST /dash/apps/:app_id/rules`, so the editor sees every document, bind, CEL, field and rate-limit issue a save would report. Each error names the rule it came from:

```json
{
  "ok": false,
  "errors": [
    {
      "in": ["goals", "allow", "view"],
      "message": "invalid CEL expression: ..."
    }
  ]
}
```

`in` is the path to the offending rule: `[etype, "allow", action]` or `[etype, "fields", field]`; it is empty for document-level errors. An invalid draft is a normal editor state, so the endpoint answers `200` with `ok: false` rather than `400`. Nothing is written: neither `rules` nor `rule_versions` changes.

## `POST /dash/apps/:app_id/tokens/rotate`

Replaces the app's admin token with a server-generated one. Requires the app member role **admin** or higher, using a dashboard session, personal access token or OAuth management token with `apps-write`; app admin tokens cannot rotate themselves.

The request body is empty. The response carries the new token exactly once:

```json
{ "adminToken": "3f2a1b4c-5d6e-4f70-8192-a3b4c5d6e7f8" }
```

Rotation is one atomic upsert in `app_admin_tokens`, so the previous token stops authenticating as soon as the response is written. The older `POST /dash/apps/:app_id/tokens` endpoint, which accepts a client-generated `admin-token`, remains available for compatibility.

## Operator flags

These endpoints require a dashboard superuser, checked the same way as `GET /dash/check-admin`: the bearer must be a live dashboard refresh token whose email is the configured superuser or listed in the `team-emails` config-app flag. A valid non-superuser token gets `403`; a missing, invalid or expired token gets `401`.

### `GET /dash/operator/flags`

Returns every config-app flag plus the metadata the operator screen uses to render controls.

```json
{
  "flags": {
    "ephemeral-apps-enabled": false,
    "magic-code-rate-limit-per-hour": 10
  },
  "known": [
    {
      "name": "ephemeral-apps-enabled",
      "type": "boolean",
      "description": "Enables unauthenticated ephemeral app creation."
    }
  ]
}
```

`flags` contains only settings that are currently set. `known` lists every flag the server reads, whether or not it is set.

### `PUT /dash/operator/flags/:name`

Creates or replaces one flag.

```json
{ "value": { "mode": "closed" } }
```

The `value` member is required and may be any JSON value. Writes use the same native config-app transaction path as `instant-server flags set`, so WAL consumers invalidate their cached policy reads.

### `DELETE /dash/operator/flags/:name`

Deletes one flag. A missing flag is a no-op.

```json
{ "name": "ephemeral-apps-enabled", "deleted": true }
```

`deleted` is `false` when the setting did not exist.

## Owner backup restore

### `POST /dash/apps/:app_id/backups/:backup_id/restore`

Restores a stored backup into a new app in the same workspace. The caller must
own the source app. The source app and its data are not changed; webhooks, OAuth
clients and files are not copied. Backups do not contain file blobs, so a
backup with `$files` entities cannot be restored until file copying is added.

```json
{ "title": "Source app (restored 2026-10-26)" }
```

`title` is optional. When omitted, the server uses the source app title plus the
current UTC date. The response returns the new app and the queued restore job:

```json
{
  "app": { "id": "3f2a1b4c-5d6e-4f70-8192-a3b4c5d6e7f8", "title": "Restored app" },
  "job": { "id": "8b1d2c3e-4f50-4612-8374-95a6b7c8d9e0" }
}
```

The server reads the backup's object prefix, stages an equivalent restore ZIP,
and starts the existing restore worker. Poll the job status through
`GET /dash/apps/:app_id/restore-jobs`.

### `GET /dash/apps/:app_id/restore-jobs`

Lists recent restore jobs where the app is either the source or the target.
Requires app membership.

```json
{ "restore-jobs": [ { "id": "8b1d2c3e-4f50-4612-8374-95a6b7c8d9e0", "app_id": "...", "title": "Restored app", "job_status": "completed" } ] }
```

Jobs are ordered newest first and limited to 50.
