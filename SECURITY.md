# Security policy

## Reporting a vulnerability

Please report suspected vulnerabilities privately through this repository's
**Security** tab ("Report a vulnerability"), which opens a private GitHub
security advisory. Do not open a public issue.

Include:

- the affected commit or image digest,
- a reproduction that uses disposable resources only,
- the authorization boundary you expected and what you observed.

Never include real credentials, user records, live access tokens or exploit
payloads containing real data. The maintainers triage reports as they arrive;
no response-time guarantee is currently offered.

## Supported versions

Only the latest commit on the default branch is supported. There are no
long-term support releases yet.

## Deployment model and trust boundaries

BackendRelay is designed to run on infrastructure you control, serving your own
applications.

- **Hosted functions and jobs** run code supplied by trusted team members. The
  Docker runner is **not a sandbox for hostile third-party code**. Anyone who can
  deploy functions must be trusted with the application's secrets and outbound
  network access.
- **Keep internal surfaces private:** the runner API, the Docker socket, database
  ports, the Prometheus metrics endpoint and monitoring dashboards belong on a
  private network, never on the public internet.
- **Public SDK CORS is intentional.** Client SDKs call the API from any origin;
  permission rules, not CORS, protect your data. Write rules for every namespace
  your apps expose.
- **Production must keep authentication rate limits enabled**, including limits on
  sending and verifying magic codes.
- **Trusted proxies:** set the trusted proxy ranges to the networks of your own
  TLS proxy only; otherwise client IP-based limits can be spoofed. With the
  bundled Caddy stack, `auto` resolves `INSTANT_TRUSTED_PROXY_HOST` and trusts
  only those host routes instead of the whole Docker network.

## If credentials are compromised

Rotate immediately: revoke affected dashboard sessions and app admin and refresh
tokens, replace runner and operator credentials and affected application secrets,
and rotate OAuth client secrets and signing keys with their providers. Confirm
that existing sessions are denied, not only new connections.

Backups can contain database contents, secrets and encryption keys. Encrypt them
before they leave the host, keep decryption keys separate from the backups, and
test restores only on disposable, isolated infrastructure. Keep any key that a
retained backup depends on until that backup has expired or been re-encrypted.
