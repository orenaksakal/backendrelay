# Origin certificates

Put `origin.pem` and `origin-key.pem` here when `DEPLOY_TLS_DIR` points at this
directory (the local dry run does; production normally uses
`/etc/backendrelay/tls`). The compose file mounts the directory read-only at
`/etc/caddy/tls`.

`*.pem`, `*.key` and `*.crt` are git-ignored. Never commit a private key.
