# TanStack Start example

A todo and chat app built with TanStack Start, using the `@instantdb/react` client and the `@instantdb/admin` SDK on the server.

## Using with BackendRelay

1. Copy `.env.example` to `.env` and set `VITE_BACKEND_URL` to your server, for example `http://localhost:8888`.
2. Run `migrate up` and start the server. See [docs/getting-started.md](../../docs/getting-started.md).
3. Create an app in the dashboard and copy its app ID and admin token into `VITE_INSTANT_APP_ID` and `INSTANT_APP_ADMIN_TOKEN`.
4. Run `pnpm install` and `pnpm dev`.

The client and admin SDKs both derive their WebSocket URL from `VITE_BACKEND_URL`. `INSTANT_APP_ADMIN_TOKEN` is server-side only; never expose it in browser code.
