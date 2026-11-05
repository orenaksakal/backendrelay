import { init } from "@instantdb/react";
import schema from "../instant.schema";

const backendURL = import.meta.env.VITE_BACKEND_URL;

if (!backendURL) {
  throw new Error("VITE_BACKEND_URL is not set");
}

export const clientDb = init({
  appId: import.meta.env.VITE_INSTANT_APP_ID!,
  schema,
  useDateObjects: true,
  firstPartyPath: "/api/instant",
  apiURI: backendURL,
  websocketURI:
    backendURL.replace(/^http/, "ws").replace(/\/$/, "") + "/runtime/session",
});
