# Expo example

A small color picker app built with Expo and the `@instantdb/react-native` client.

## Using with BackendRelay

1. Copy `.env.example` to `.env` and set `EXPO_PUBLIC_BACKEND_URL` to your server, for example `http://localhost:8888`. On a physical device, use your machine's LAN address instead of `localhost`.
2. Run `migrate up` and start the server. See [docs/getting-started.md](../../docs/getting-started.md).
3. Create an app in the dashboard and copy its app ID and admin token into `EXPO_PUBLIC_INSTANT_APP_ID` and `INSTANT_APP_ADMIN_TOKEN`.
4. Run `pnpm install` and `pnpm start`.

The SDK derives its WebSocket URL from `EXPO_PUBLIC_BACKEND_URL`. `INSTANT_APP_ADMIN_TOKEN` is only for server-side code; this example does not use it at runtime.
