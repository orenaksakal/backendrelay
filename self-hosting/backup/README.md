# pgBackRest physical backups

`recovery.py` still produces the independent, encrypted logical dumps described
in [`docs/self-hosting.md`](../../docs/self-hosting.md#backups-and-recovery).
This directory documents the second line: continuous WAL archiving plus weekly
full and daily differential physical backups with [pgBackRest](https://pgbackrest.org),
taken from the PostgreSQL image built in [`../postgres/`](../postgres/).

Everything is configured through environment variables. pgBackRest reads
`PGBACKREST_<OPTION>` directly, so no repository credential lives in a committed
configuration file. The compose files keep only fixed, non-empty settings in
`environment:`; repository options and secrets come from `pgbackrest.env`,
which Compose includes when it exists, and archive mode is **off** until you
opt in.

## One-time setup

1. Create an R2 bucket for repo 1, for example `instantdb-pgbackrest`, with no
   public access. Optional: create a bucket at a second S3-compatible provider
   (Backblaze B2 or Hetzner Object Storage) for repo 2.
2. Create S3 credentials scoped to those buckets. Copy
   `self-hosting/pgbackrest.env.example` to `self-hosting/pgbackrest.env`
   (mode `0600`) and fill in repo 1: the endpoint is
   `https://<account>.r2.cloudflarestorage.com`, with region `auto`,
   uri-style `path`, path `/pgbackrest`, the bucket, key, key-secret, and a
   strong cipher pass. Compose includes the file only when it exists, so only
   variables you put in it reach the container.
   Repo 2 is optional; its block is commented out in the example. Uncomment it
   and fill in the same option set with the `repo2-` prefix and its own cipher
   pass. Never leave a `PGBACKREST_*` variable defined but empty: pgBackRest
   rejects empty values.
3. Restart PostgreSQL so `pgbackrest.env` reaches the container:
   `docker compose up -d --force-recreate postgres` from `self-hosting/` (or
   `--file` your deployment compose file). Archiving is still off.
4. Create the stanza and verify connectivity:
   ```sh
   docker compose exec -T -u postgres postgres pgbackrest --stanza=backendrelay stanza-create
   docker compose exec -T -u postgres postgres pgbackrest --stanza=backendrelay check
   ```
5. Set `INSTANT_PG_ARCHIVE_MODE=on` in `self-hosting/.env` and restart
   PostgreSQL again (`docker compose up -d --force-recreate postgres`).
   `archive_mode` is a restart-only setting, which is why it is a separate step.
6. Confirm that WAL segments are arriving, then take the first full backup. A
   physical backup waits for its stop WAL segment to reach the repository, so
   take it only after archiving is on:
   ```sh
   docker compose exec -T -u postgres postgres pgbackrest --stanza=backendrelay info
   docker compose exec -T -u postgres postgres pgbackrest --stanza=backendrelay --type=full backup
   ```
7. Install the schedule:
   ```sh
   sudo systemctl enable --now backendrelay-pgbackrest-full.timer backendrelay-pgbackrest-diff.timer
   ```
   The units assume the repository is at `/opt/backendrelay`; set
   `COMPOSE_FILE` in `/etc/backendrelay/pgbackrest.env` when the deployment uses
   another compose file. Put a healthchecks.io ping URL in the same file as
   `PGBACKREST_HEALTHCHECK_URL`; each successful backup pings it and a failed
   ping never fails the unit. The existing `BackupStale` alert and
   `WalArchiveFailing` alert keep their meaning.

The spool volume (`pgbackrest-spool`, mounted at `/var/spool/pgbackrest`) must
not live inside the PostgreSQL data directory. The image's entrypoint fixes its
ownership before PostgreSQL drops privileges.

## Schedule and retention

- Sunday 02:30 UTC: full backup (`backendrelay-pgbackrest-full.timer`).
- Monday to Saturday 02:30 UTC: differential backup
  (`backendrelay-pgbackrest-diff.timer`).
- `repo1-retention-full-type=time` and `repo1-retention-full=14` keep 14 days of
  full backups and their dependent differentials and WAL. Repo 2 uses the same
  retention when it is configured.
- `compress-type=zst`, `process-max=2`, `start-fast=y` and `archive-async=y`.
  The last option spools WAL in the named volume so the archiver never blocks
  PostgreSQL on a slow object store. `INSTANT_PG_ARCHIVE_TIMEOUT` (default 60
  seconds) bounds the RPO for an idle database.

## Inspect

```sh
docker compose exec -T -u postgres postgres pgbackrest --stanza=backendrelay info
docker compose exec -T -u postgres postgres pgbackrest --stanza=backendrelay check
```

## Restore

Always restore into a disposable container and volume, never over the live data
directory. Use the same PostgreSQL image and the same `pgbackrest.env`
repository settings as the source deployment.

```sh
# Empty target volume and a disposable container in the same network. Copy
# pgbackrest.env to /etc/backendrelay/pgbackrest-restore.env for the repository
# variables; the stanza and data directory settings match the compose service.
docker volume create backendrelay-restore-data
docker run -d --name backendrelay-restore \
  --env-file /etc/backendrelay/pgbackrest-restore.env \
  -e PGBACKREST_STANZA=backendrelay \
  -e PGBACKREST_PG1_PATH=/var/lib/postgresql/data \
  -e PGBACKREST_PG1_USER=postgres \
  -v backendrelay-restore-data:/var/lib/postgresql/data \
  backendrelay/postgres:17-local sleep infinity
docker exec -u root backendrelay-restore chown postgres:postgres /var/lib/postgresql/data

# Latest backup.
docker exec -u postgres backendrelay-restore pgbackrest --stanza=backendrelay restore
docker exec -u postgres backendrelay-restore pg_ctl -D /var/lib/postgresql/data start

# Or, instead of the latest backup: point in time, promoted at the target.
docker exec -u postgres backendrelay-restore pgbackrest --stanza=backendrelay \
  --type=time --target="2026-10-26 12:00:00+00" --target-action=promote restore
docker exec -u postgres backendrelay-restore pg_ctl -D /var/lib/postgresql/data start
```

`pgbackrest restore` needs the target data directory to be empty (or pass
`--delta` for an existing restored copy). Point-in-time restores replay WAL up
to the target; `--target-action=promote` makes the recovered server writable.
Verify the restored database before switching traffic, for example by running
`pg_isready`, `pg_restore --list` on a `recovery.py` dump, and the server's
health endpoint against a disposable compose project.

When a physical backup is unavailable or you need original app identities,
fall back to the encrypted logical bundle from `recovery.py`; it stays the
independent copy that also survives a pgBackRest repository loss.

## Secrets backup

`server_secrets` (`/home/instant/config/secrets.env`) is not part of a physical
backup. `recovery.py` includes it in the logical bundle, but P7.4 keeps an
independent age-encrypted copy in R2 and offline so that a lost bundle does not
lose the only keyset:

```sh
sudo SECRETS_AGE_RECIPIENT='age1...' \
  SECRETS_UPLOAD_COMMAND='rclone copy "$BUNDLE" r2:instantdb-secrets/' \
  SECRETS_VERIFY_COMMAND='rclone lsf r2:instantdb-secrets/ | grep -qx "$BUNDLE_NAME"' \
  bash self-hosting/backup/secrets-backup.sh
```

Use the dedicated `instantdb-secrets` bucket, not `instantdb-backups` (that
bucket's lifecycle expires every object after 30 days) and not the pgBackRest
repository.

[`secrets-backup.sh`](secrets-backup.sh) copies the `server_secrets` volume
(or `SECRETS_VOLUME`), encrypts it with age, writes a `.sha256` sidecar under
`/var/lib/backendrelay/secrets-backups`, and never prints a value. Passing
`SECRETS_AGE_IDENTITY_FILE` makes it decrypt the new bundle and compare it with
the payload. The identity stays offline; the recipient is public. See
[Secrets](../../docs/operations.md#secrets) for the inventory, the restore
procedure and the monthly verification.

## Drill

[`pgbackrest-drill.sh`](pgbackrest-drill.sh) runs the whole cycle locally against the image, with no cloud credentials. It uses a throwaway encrypted `posix` repository: archive WAL, take a full and a differential backup, delete every row, then restore to the second before the delete and check that every row is back. Run it after changing the image or the pgBackRest settings: `bash self-hosting/backup/pgbackrest-drill.sh` (set `IMAGE=` to test a pushed image). The repository is deliberately `posix` rather than S3 because pgBackRest's S3 driver requires TLS, while a local S3 emulator serves plain HTTP; the real-host `pgbackrest check` and full backup against R2 are part of the [monitoring go-live checks](../../docs/operations.md#pgbackrest-from-the-real-host).
