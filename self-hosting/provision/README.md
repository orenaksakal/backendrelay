# Production provisioning (P7.3)

This directory prepares the production host: the CAX41 rebuild, host hardening,
Cloudflare DNS/WAF, R2 buckets, self-managed PostgreSQL, Cloudflare Email
Sending and GHCR pull access. Everything here is **owner-gated**: the commands
need a Hetzner account, a Cloudflare account, GitHub credentials or the real
host. The files in this directory are the repo half; run them and copy the
results into `self-hosting/deploy/.env` (P7.4, P7.6).

| File | Purpose |
| --- | --- |
| `host-bootstrap.sh` | Idempotent Ubuntu 24.04 host setup: packages, key-only SSH, `ufw`, Docker with the P2.11 log limits, unattended security updates, directories. `--check` verifies without changing anything. |
| `docker-daemon.json` | Host-wide Docker defaults: `local` log driver, 50m × 5 files, `no-new-privileges`. The same limits are set per service in every compose file. |
| `README.md` | The ordered runbook below. |

Placeholders used below:

| Placeholder | Meaning |
| --- | --- |
| `<domain>` | The BackendRelay platform domain, e.g. `backendrelay.example` |
| `<origin-ipv4>`, `<origin-ipv6>` | The CAX41's public addresses from the Hetzner console |
| `<admin-ip>` | The public address you SSH from, usually `<ipv4>/32` |
| `<account-id>` | The Cloudflare account ID (Dashboard → account home) |
| `<github-owner>` | The GitHub account or organisation that owns the private images |

Order matters: rebuild and firewall first, then DNS, then the services that
the deploy kit expects to find.

---

## 1. Rebuild the CAX41

The old server ran a Coolify deployment. Rebuilding replaces the disk, so the
old deployment, its database and its secrets are destroyed — that is the point.
Do not carry any of them forward, and do not reuse their passwords or keys:
treat the old database as compromised (PLAN §P1).

1. In Hetzner Cloud Console, open the server, then **Rebuild** → **Ubuntu
   24.04**. A rebuild keeps the server type, location and price; confirm both
   the CAX41 type and the legacy price shown in the dialog before continuing.
2. Select the admin SSH key. Do not enable a root password. Note the IPv4 and
   IPv6 addresses after the rebuild.
3. Delete the old Coolify DNS records and any tunnel or proxy configuration
   that pointed at the previous installation.
4. Log in with the key: `ssh -i ~/.ssh/<key> root@<origin-ipv4>`.

Nothing else is copied from the old host. A clean image is the baseline.

## 2. Hetzner Cloud Firewall

Create the firewall before the host starts serving, and attach it to the
server. This is the authoritative packet filter: Docker publishes container
ports through its own iptables chain, which bypasses `ufw`'s INPUT rules.

| Direction | Protocol | Port | Source | Purpose |
| --- | --- | --- | --- | --- |
| Inbound | TCP | 22 | `<admin-ip>` | Admin SSH |
| Inbound | TCP | 80 | Cloudflare IPv4 + IPv6 ranges | HTTP (redirect and ACME if used) |
| Inbound | TCP | 443 | Cloudflare IPv4 + IPv6 ranges | Public HTTPS |
| Inbound | TCP | 8443 | Cloudflare IPv4 + IPv6 ranges | Staging HTTPS (P7.5; production keeps 443) |
| Outbound | All | — | Any | Package mirrors, GHCR, Cloudflare, R2, backups |

Copy the ranges from <https://www.cloudflare.com/ips-v4> and
<https://www.cloudflare.com/ips-v6>; the bootstrap script applies the same list
to `ufw`. If your admin address changes, update both firewalls.

The staging project on the same host binds 8080/8443 instead of 80/443, because
production owns those ports. `host-bootstrap.sh` only manages 80/443, so add the
8443 rule yourself and keep it in sync with the Cloudflare ranges:

```sh
for cidr in <cloudflare-ranges>; do
  sudo ufw allow from "$cidr" to any port 8443 proto tcp comment 'cloudflare staging https'
done
```

Check the firewall from outside before continuing. SSH is the only direct
inbound path; 80/443 answer only through Cloudflare once DNS is proxied, so a
direct `curl http://<origin-ipv4>/` timing out is the expected result:

```sh
ssh -o ConnectTimeout=5 root@<origin-ipv4> true
```

## 3. Host bootstrap

```sh
sudo ADMIN_SSH_CIDR='<admin-ip>' \
  bash /opt/backendrelay/self-hosting/provision/host-bootstrap.sh
```

The checkout does not exist yet at this point, so clone the repository into
`/opt/backendrelay` first (install `git` with `apt-get install -y git` if the
image lacks it, or copy `self-hosting/provision/` over with `scp`). The script:

- installs `docker.io`, `docker-compose-v2`, `curl`, `python3`, `age`, `ufw` and
  `unattended-upgrades` from the Ubuntu archive;
- writes `/etc/ssh/sshd_config.d/10-backendrelay.conf` so SSH is key-only,
  before it touches the firewall, and refuses when no authorized key exists;
- denies inbound traffic except SSH from `ADMIN_SSH_CIDR` and 80/443 from the
  current Cloudflare ranges;
- installs `docker-daemon.json` (log limits, `no-new-privileges`) and restarts
  Docker only while no containers run;
- enables unattended security updates. Automatic reboot stays off because a
  reboot interrupts live sessions; schedule reboots with the drain procedure in
  [`docs/operations.md`](../../docs/operations.md#restart-with-a-drain);
- creates `/opt/backendrelay`, `/etc/backendrelay` (0700) and
  `/var/lib/backendrelay` (0700), sets UTC and enables NTP.

Verify:

```sh
sudo ADMIN_SSH_CIDR='<admin-ip>' \
  bash /opt/backendrelay/self-hosting/provision/host-bootstrap.sh --check
```

`cosign` is not installed by the script because `deploy.sh` verifies signatures
with it. Install the release you trust from
<https://github.com/sigstore/cosign/releases>, verify its checksum, and put the
binary in `/usr/local/bin`. `--check` warns while it is missing.

`ufw` rules do not follow Cloudflare when the published ranges change. Re-run
the script quarterly (it only adds missing rules); remove stale ranges with
`ufw status numbered` and `ufw delete <number>`.

## 4. Cloudflare DNS, TLS and WAF

### DNS

In the zone for `<domain>`:

| Name | Type | Content | Proxy |
| --- | --- | --- | --- |
| `api` | `A` | `<origin-ipv4>` | Proxied (orange) |
| `api` | `AAAA` | `<origin-ipv6>` | Proxied |
| `dash` | `A` | `<origin-ipv4>` | Proxied |
| `dash` | `AAAA` | `<origin-ipv6>` | Proxied |
| `staging-api` | `A` | `<origin-ipv4>` | Proxied; Origin Rule to port 8443 |
| `staging-api` | `AAAA` | `<origin-ipv6>` | Proxied; Origin Rule to port 8443 |
| `staging-dash` | `A` | `<origin-ipv4>` | Proxied; Origin Rule to port 8443 |
| `staging-dash` | `AAAA` | `<origin-ipv6>` | Proxied; Origin Rule to port 8443 |
| `files` | created by the R2 custom domain in step 5 | — | Proxied automatically |

Do not create `files` by hand: the R2 custom-domain binding owns the record, and
a manual record blocks it. The `staging-*` records are only needed for P7.5;
their two Origin Rules (**Rules → Origin Rules → Destination Port `8443`**)
reach the staging Caddy container, and the origin certificate must cover those
names too (one certificate for all four names is simplest).

Set **SSL/TLS → Overview → Full (strict)**. Create a Cloudflare **origin
certificate** for `api.<domain>` and `dash.<domain>` and place `origin.pem` and
`origin-key.pem` in `DEPLOY_TLS_DIR` (`/etc/backendrelay/tls`, mode 0700) as
described in [`deploy/tls/README.md`](../deploy/tls/README.md). Enable **Always
Use HTTPS** and set the minimum TLS version to 1.2.

### WAF rate limits

Add two rate-limiting rules under **Security → WAF → Rate limiting rules**,
before any rule that would skip them. Both count per client IP over one minute
and block for one minute; that stops floods without touching normal token
refresh or dashboard polling.

| Rule | Expression | Requests | Action |
| --- | --- | --- | --- |
| `auth-flood` | `(http.request.uri.path matches "^/runtime/auth/")` | 120 / 60 s | Block, 60 s |
| `dash-flood` | `(http.request.uri.path matches "^/dash/")` | 120 / 60 s | Block, 60 s |

The server also caps bodies and live sessions itself (P2.8) and Caddy caps
management bodies, so these are the outer limit, not the only one. Add
`/storage/*` to the dashboard rule if uploads are abused; legitimate uploads are
much rarer than dashboard polls.

## 5. R2 buckets, domains and tokens

The buckets use the same names as `deploy/.env`:

| Bucket | Purpose | Lifecycle |
| --- | --- | --- |
| `instantdb-files` | User uploads, served from `files.<domain>` | None |
| `instantdb-backups` | Per-app backups from the dashboard | Expire after 30 days (`r2-wrangler.sh configure-lifecycle`) |
| `instantdb-pgbackrest` | pgBackRest repository 1 (step 6) | pgBackRest retention (14 days of fulls) |
| `instantdb-secrets` | age-encrypted `server_secrets` copies (P7.4) | None; never expire |

Create them and apply CORS and lifecycle with the existing tool. It uses your
Wrangler login and never reads the S3 keys:

```sh
cd self-hosting
R2_FILES_BUCKET=instantdb-files R2_BACKUPS_BUCKET=instantdb-backups \
  ./r2-wrangler.sh ready https://dash.<domain>
npx wrangler r2 bucket create instantdb-pgbackrest
npx wrangler r2 bucket create instantdb-secrets
```

Then attach the files custom domain: **R2 → instantdb-files → Settings →
Custom domains → Connect domain** → `files.<domain>`. Wait until Cloudflare
shows the certificate as active. In `deploy/.env`, `S3_ENDPOINT` is
`https://<account-id>.r2.cloudflarestorage.com`, while `S3_PUBLIC_ENDPOINT` and
`INSTANT_STORAGE_ORIGIN` become `https://files.<domain>`.

`r2-wrangler.sh` writes one dashboard origin. Additional app origins are added
when an app is onboarded (P8.3): extend `self-hosting/r2-cors.json` with the new
origin and re-run `configure-cors`, or set the list in the R2 console.

Create **scoped S3 API tokens** (R2 → Manage R2 API Tokens), one per consumer,
each limited to Object Read & Write on only the buckets in the table:

| Token | Buckets | Goes into |
| --- | --- | --- |
| `backendrelay-server` | `instantdb-files` and `instantdb-backups` | `S3_ACCESS_KEY_ID` / `S3_SECRET_ACCESS_KEY` |
| `backendrelay-pgbackrest` | `instantdb-pgbackrest` | `PGBACKREST_REPO1_S3_KEY` / `PGBACKREST_REPO1_S3_KEY_SECRET` |
| `backendrelay-secrets` | `instantdb-secrets` | an rclone/S3 config owned by root, mode 0600, used only by `secrets-backup.sh` |

The server reads `S3_FILES_BUCKET` and `S3_BACKUPS_BUCKET` with one key pair, so
that pair must cover both buckets and nothing else. Never put a bucket-wide
account token on the host. Store the secrets in the password manager and in the
mode-0600 files from P7.4, never in git.

## 6. Database (D4 = A: self-managed PostgreSQL)

D4 is A: PostgreSQL 17 on this box. PlanetScale (D4 = B) is not used, so skip
its checklist; [`docs/managed-postgres.md`](../../docs/managed-postgres.md)
keeps it for a future move.

1. Put the digest-pinned image built in P5.5 in `INSTANT_POSTGRES_IMAGE`. It
   contains PostgreSQL 17, wal2json, pg_hint_plan and pgBackRest.
2. Copy [`pgbackrest.env.example`](../pgbackrest.env.example) to
   `self-hosting/pgbackrest.env` (mode 0600) and fill in repo 1 from the token
   above: endpoint `https://<account-id>.r2.cloudflarestorage.com`, region
   `auto`, path-style, bucket `instantdb-pgbackrest`, cipher passphrase. Add
   repo 2 (Backblaze B2 or Hetzner Object Storage) when available.
3. Start PostgreSQL once with `INSTANT_PG_ARCHIVE_MODE=off` (the default), run
   `stanza-create` and `pgbackrest check`, then set `INSTANT_PG_ARCHIVE_MODE=on`
   and recreate the container. Take one full backup before the first deploy.
   The full sequence is in [`self-hosting/backup/README.md`](../backup/README.md).
4. Leave `INSTANT_PG_SHARED_BUFFERS`, `INSTANT_PG_EFFECTIVE_CACHE_SIZE` and the
   container limits at the `.env.example` values for a 32 GB CAX41
   (`shared_buffers` 6 GB, `effective_cache_size` 16 GB). The compose file sets
   `max_connections=200`, `shared_preload_libraries` and `log_min_duration_statement`
   from P3.10; migration 151 applies the `triples` autovacuum overrides. The
   image's init script creates the unprivileged role on a fresh volume.
5. Confirm the role after the first `migrate up`:

   ```sh
   cd /opt/backendrelay/self-hosting
   docker compose --project-directory . --env-file deploy/.env \
     -f deploy/docker-compose.yml exec -T postgres \
     psql -U instant -d instant \
     -c "SELECT rolsuper FROM pg_roles WHERE rolname = current_user;"
   ```

   It returns `f`.

The server never applies migrations at startup; `deploy.sh` runs `migrate up`
(P7.2). An empty database needs no backup, which is what `DEPLOY_FIRST_DEPLOY=1`
covers.

## 7. Email: Cloudflare Email Sending (D6)

1. In the Cloudflare dashboard open **Email Service → Email Sending** for
   `<domain>`, add the platform sender domain and enable it. Cloudflare adds the
   SPF and DKIM records it needs. Add a DMARC record if the zone has none:
   `_dmarc.<domain> TXT "v=DMARC1; p=none; rua=mailto:dmarc@<domain>"`, then
   tighten `p` once reports are clean.
2. Create an API token under **My Profile → API Tokens** with Email Sending
   Edit, plus Zone Read and Email Sending Read on `<domain>`. It is read by the
   server only; keep it out of Wrangler and Caddy.
3. Set in `deploy/.env`:

   ```sh
   INSTANT_EMAIL_PROVIDER=cloudflare
   CLOUDFLARE_ACCOUNT_ID=<account-id>
   CLOUDFLARE_EMAIL_API_TOKEN=<token>
   # Startup requires the transactional sender.
   INSTANT_TRANSACTIONAL_EMAIL_SENDER_EMAIL=login@<domain>
   INSTANT_DASHBOARD_EMAIL_SENDER_EMAIL=login@<domain>
   INSTANT_APP_EMAIL_SENDER_EMAIL=login@<domain>
   INSTANT_TEAM_EMAIL_SENDER_EMAIL=login@<domain>
   ```

   The server refuses to start when the provider is `cloudflare` and any of the
   account ID, token or transactional sender is missing. Per-app domains are
   added at onboarding (P2.17, P8.3); until then apps send from
   `INSTANT_APP_EMAIL_SENDER_EMAIL`.
4. After the first deploy, request a dashboard sign-in code and confirm delivery
   and that SPF, DKIM and DMARC pass in the received message. A failure shows as
   `EmailFailing` (see [`docs/operations.md`](../../docs/operations.md#emailfailing)).

## 8. GHCR pull access

The release images are private. The host needs a token that can read packages,
not push:

1. Create a GitHub fine-grained token for `<github-owner>` with **Packages:
   Read** on the `server`, `dashboard`, `postgres` and `canary` packages (or a
   classic token with `read:packages`).
2. Log in as the account that runs `deploy.sh` — root on the production host:

   ```sh
   printf '%s' '<token>' | sudo docker login ghcr.io -u <github-owner> --password-stdin
   ```

   This writes `/root/.docker/config.json` (0600). Store the token in the
   password manager; rotate it when CI tokens rotate.
3. Verify a real release digest pulls:

   ```sh
   sudo docker pull ghcr.io/<github-owner>/<server-image>@sha256:<digest>
   ```

4. Put the same digest-only references in `INSTANT_SERVER_IMAGE`,
   `INSTANT_DASHBOARD_IMAGE`, `INSTANT_POSTGRES_IMAGE`, `CADDY_IMAGE`,
   `DEPLOY_SMOKE_IMAGE` and `CANARY_IMAGE`. `deploy.sh` still verifies the
   signatures with the cosign public key; registry auth alone is not enough.

## 9. Secrets (P7.4)

The signed server image can generate the keysets without any server
configuration. Do this once, before the first `serve`:

```sh
cd /opt/backendrelay/self-hosting
umask 077
docker compose --project-name backendrelay --project-directory . \
  --env-file deploy/.env -f deploy/docker-compose.yml \
  run --rm --no-deps -T server generate-secrets \
  > /tmp/secrets.env
```

`backendrelay` is the default `DEPLOY_COMPOSE_PROJECT`; the volume name and the
backup script's default depend on it.

Store the file in the password manager, copy it into the `server_secrets` volume
with the one-shot command in [`docs/operations.md#secrets`](../../docs/operations.md#secrets),
and remove the temporary. Keep `deploy/.env` at mode 0600. Then create the age
key pair, keep the identity offline, and run
[`backup/secrets-backup.sh`](../backup/secrets-backup.sh) once with an R2 upload
and once to offline media. The inventory of which secret lives where is in the
same operations section.

Do not generate a second `secrets.env` after the server has stored ciphertext;
rotate instead ([Key rotation](../../docs/operations.md#key-rotation)).

## 10. Hand over

```sh
sudo ADMIN_SSH_CIDR='<admin-ip>' \
  bash /opt/backendrelay/self-hosting/provision/host-bootstrap.sh --check
```

Then:

- P7.5: stand up the second compose project as `staging` — own buckets, subnet,
  ports, database and keysets — and rehearse one release and one rollback there.
  See [`../deploy/README.md#staging-on-the-same-host-p75`](../deploy/README.md#staging-on-the-same-host-p75).
- P7.6: run `deploy.sh` with the first signed release, restrict dashboard
  emails, pin the initial config flags, run `instant-server audit`, create the
  canary app. The ordered steps and checklist are in
  [`docs/operations.md#first-production-deploy-p76`](../../docs/operations.md#first-production-deploy-p76).
- P7.7: bring up monitoring and fire a test alert to the phone.

### Provisioning checklist

- [ ] CAX41 rebuilt from Ubuntu 24.04; old Coolify deployment and its secrets discarded
- [ ] Hetzner Cloud Firewall attached: 22 from admin, 80/443 from Cloudflare, 8443 from Cloudflare for staging
- [ ] `host-bootstrap.sh --check` passes; `cosign` installed
- [ ] `api.`, `dash.` proxied; SSL Full (strict); origin certificate in place
- [ ] Two WAF rate-limit rules active
- [ ] `instantdb-files`, `instantdb-backups`, `instantdb-pgbackrest`, `instantdb-secrets` created
- [ ] `files.<domain>` active as an R2 custom domain; CORS and lifecycle applied
- [ ] Bucket-scoped R2 tokens stored; no account-wide token on the host
- [ ] pgBackRest stanza created, `check` passes, one full backup taken
- [ ] Cloudflare Email Sending verified; SPF, DKIM and DMARC pass
- [ ] GHCR login works for the deploy user; a release digest pulls
- [ ] `secrets.env` generated, installed in `server_secrets`, and stored in the password manager
- [ ] age key pair created; identity offline; encrypted secrets copy in R2 and offline
- [ ] `.env` filled, mode 0600, and `deploy.sh` rehearsed on staging
- [ ] `INSTANT_DASHBOARD_ALLOWED_EMAILS` and `INSTANT_SUPERUSER_EMAIL` set to the owner; signup closed
- [ ] First production deploy ran with `DEPLOY_FIRST_DEPLOY=1` and `DEPLOY_SKIP_CANARY_SMOKE=1`
- [ ] `flags set ephemeral-apps-enabled false` and `flags set app-metric-slugs {...}`; server restarted
- [ ] Production canary app and root-owned token file created; deploy without the skip passes
- [ ] `instant-server audit` reports zero errors; an unlisted address is refused at dashboard sign-in
- [ ] `staging-api.`/`staging-dash.` proxied with Origin Rules to 8443; `deploy/.env.staging` filled (0600)
- [ ] `instantdb-staging-files`, `instantdb-staging-backups` created with a bucket-scoped token
- [ ] Staging keysets generated into `backendrelay-staging_server_secrets`; staging canary app and token created
- [ ] One release and one deliberate rollback rehearsed on staging
