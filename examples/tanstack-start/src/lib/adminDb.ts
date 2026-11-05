import { init } from "@instantdb/admin";
import schema from "../instant.schema";

const appId = process.env.VITE_INSTANT_APP_ID!;
const adminToken = process.env.INSTANT_APP_ADMIN_TOKEN;
const backendURL = process.env.BACKEND_URL || process.env.VITE_BACKEND_URL;

if (!backendURL) {
  throw new Error("VITE_BACKEND_URL is not set");
}

export const adminDb = init({
  appId,
  adminToken,
  schema,
  useDateObjects: true,
  apiURI: backendURL,
});
