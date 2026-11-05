# Lifeboat on Fly.io Frankfurt or Railway

A lifeboat is the same signed server digest as production, running outside the
production host, against a restored or point-in-time copy of the database and
with the production `server_secrets`. It answers one question: if the CAX41 is
gone, can BackendRelay serve existing app identities somewhere else?

This directory supplies a Fly.io template. Railway needs no committed file: it
deploys the same image and reads variables from its dashboard or CLI. Pick one
provider for the drill; do not run both.

Everything below is owner-gated work. The repository can prepare the template,
but creating accounts, databases, volumes and DNS records is an owner action.

The production image is private in GHCR. The provider must be able to pull the
digest: configure a read-only registry credential in the provider, or push a copy
to a registry it can read. Do not make the production image public.

## What the lifeboat needs

| Input | Where it comes from |
| --- | --- |
| Signed server digest | The release mapping used for production. Verify it with [`verify-release-images.sh`](../../verify-release-images.sh) before deploying. |
| Database copy | `primary.dump` from a verified `recovery.py` bundle, or a point-in-time restore from the provider. |
| `server_secrets` | The three keysets from `/home/instant/config/secrets.env`. Never generate new ones. |
| Server environment | `self-hosting/deploy/.env.example`, with the database URL and public hostnames changed. |
| Object storage | The production bucket names and a scoped token, or a copy of the bucket. Restored `$files` rows point at production keys. |
| Test hostname | `lifeboat-api.<domain>`, proxied or direct depending on the provider. |
| Email | A provider token and sender that can deliver a magic code for the sign-in check. |

## 1. Restore the database

### From a logical bundle

Keep the connection string in a mode-0600 file and pass it through a variable;
`pg_restore` must not receive a password in argv.

```sh
# On the host that holds the decrypted bundle, with the lifeboat database
# reachable over TLS. The dump is PostgreSQL 17 custom format; use a matching
# client or run pg_restore inside a PostgreSQL 17 container.
pg_restore --exit-on-error --no-owner --no-acl \
  --dbname "$LIFEBOAT_DATABASE_URL" /srv/drills/restore/primary.dump
```

The lifeboat database must provide the extensions the production database uses,
at least `pg_hint_plan` and `pg_stat_statements`. A provider that cannot install
them is not a valid lifeboat, because the restored schema and query planner
depend on them.

Then connect as the database owner and grant the role the server will use. The
`--no-owner --no-acl` restore leaves objects owned by the restoring user.

```sql
GRANT USAGE ON SCHEMA public TO "<lifeboat-db-user>";
GRANT SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER
  ON ALL TABLES IN SCHEMA public TO "<lifeboat-db-user>";
GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA public TO "<lifeboat-db-user>";
```

Run `instant-server migrate up` once against the restored database with the same
signed image. A dump from the same release is already at the right version, so
this should be a no-op; a migration here means the backup and image do not match,
which is itself a finding.

### From point-in-time recovery

If the provider offers PITR, restore to the newest timestamp that is no later
than the backup you intended to test. Write the timestamp into the drill notes.
The restored cluster still needs the application role and its grants.

## 2. Restore `server_secrets`

The server reads `INSTANT_AEAD_KEYSET_JSON`, `INSTANT_WEBHOOK_KEYSET_JSON` and
`INSTANT_RATE_LIMIT_HMAC_KEY` from the environment, or from
`/home/instant/config/secrets.env` when they are absent. Both paths work on a
lifeboat; pick one.

**Platform secrets (simplest).** On the host that holds the decrypted
`secrets.env`, write a mode-0600 import file and remove it after the import:

```sh
sudo install -d -m 700 /srv/drills/restore/secrets
sudo tar -xf /srv/drills/restore/server-secrets.tar \
  -C /srv/drills/restore/secrets
sudo chmod 600 /srv/drills/restore/secrets/secrets.env
umask 077
set -a
. /srv/drills/restore/secrets/secrets.env
set +a
{
  printf 'INSTANT_AEAD_KEYSET_JSON=%s\n' "$INSTANT_AEAD_KEYSET_JSON"
  printf 'INSTANT_WEBHOOK_KEYSET_JSON=%s\n' "$INSTANT_WEBHOOK_KEYSET_JSON"
  printf 'INSTANT_RATE_LIMIT_HMAC_KEY=%s\n' "$INSTANT_RATE_LIMIT_HMAC_KEY"
} > /srv/drills/restore/lifeboat-secrets.env
```

Then import it with the provider's secret command (for Fly, `fly secrets import
< /srv/drills/restore/lifeboat-secrets.env`), and shred the file. Do not commit
it, paste it into a ticket, or leave it on the host after the drill.

**Volume (closest to production).** Create a provider volume, mount it at
`/home/instant/config`, and copy the restored `secrets.env` there with the
provider's file transfer (Fly's `sftp` shell or a one-off machine; Railway's
service shell or dashboard upload). The file must be mode 0600 and owned by the
`instant` user (uid 10001 in the server image).

Either way, verify a stored OAuth client sign-in before declaring the drill
successful. A new keyset would still let the server start, but old ciphertext
would fail to decrypt.

## 3. Deploy the signed image

### Fly.io Frankfurt

```sh
cd self-hosting/drills/lifeboat
cp fly.toml.example fly.toml
# Edit fly.toml: app name, image digest, backend and dashboard URLs, buckets.
fly apps create <lifeboat-app>
```

The server still needs its secret environment (database URL, S3 keys, email
token, OAuth client secret) plus the three keysets from step 2. Combine them into
one mode-0600 `lifeboat.env` and import it; do not commit it or pass a value on
the command line:

```sh
fly secrets import < /srv/drills/restore/lifeboat.env
fly deploy --config fly.toml
fly status --config fly.toml
```

The template uses `primary_region = "fra"`, `internal_port = 8888` and a
`GET /health/system` check, matching the server's Dockerfile and health contract.
Verify that the image has a build for the machine architecture you choose; a
single-architecture image will not run on the other one.

### Railway

1. Create a project and choose **Deploy from Docker Image**, then paste the
   signed digest.
2. Add the variables from the production server environment, with
   `INSTANT_BACKEND_URL` and `INSTANT_DASHBOARD_URL` set to the lifeboat
   hostnames and `DATABASE_URL` set to the restored database.
3. Set the service's target port to `8888` and add an HTTP health check to
   `/health/system`.
4. Generate a Railway domain, then add `lifeboat-api.<domain>` as a custom
   domain if the drill calls for the real test hostname.

Do not deploy the dashboard image unless the drill needs it; the plan only
requires the server image and a test hostname.

## 4. Point the test hostname and verify

- DNS: point `lifeboat-api.<domain>` at the Fly app or Railway domain. On Fly,
  `fly certs add lifeboat-api.<domain>` (or the provider's automatic
  certificate) completes TLS.
- Set `INSTANT_BACKEND_URL=https://lifeboat-api.<domain>` before the first
  request that builds absolute URLs. A mismatch breaks OAuth callbacks, storage
  URLs and webhook signatures.
- Verify in this order: `/health/system` is `200 {"wal":"ok"}`; an SDK query with
  a real app admin token returns data; a file download from a restored `$files`
  row succeeds; a magic-code sign-in for the owner completes; a WebSocket write
  produces a refresh (optional but the strongest check).
- Record the recovery time from the start of the database restore to the last
  green check. A lifeboat that starts but cannot decrypt OAuth secrets is not a
  recovery.

## 5. Tear down

```sh
fly apps destroy <lifeboat-app>
fly volumes list --app <lifeboat-app>   # destroy any volume left behind
```

For Railway, delete the service, its volume and the custom domain. Then:

- Delete the lifeboat database copy and any point-in-time restore instance.
- Revoke the provider token and any DNS token created for the drill.
- Shred the local import file and any decrypted bundle copy.
- Remove the `lifeboat-api` DNS record unless it is being kept for the next
  drill.

A lifeboat left running is a second production copy with production keys. The
drill is not finished until it is gone.
