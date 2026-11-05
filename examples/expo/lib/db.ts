import { init } from "@instantdb/react-native";
import schema from "../instant.schema";

const backendURL = process.env.EXPO_PUBLIC_BACKEND_URL;

if (!backendURL) {
  throw new Error("EXPO_PUBLIC_BACKEND_URL is not set");
}

export const db = init({
  appId: process.env.EXPO_PUBLIC_INSTANT_APP_ID!,
  schema,
  apiURI: backendURL,
  websocketURI:
    backendURL.replace(/^http/, "ws").replace(/\/$/, "") + "/runtime/session",
});
