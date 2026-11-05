/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** `1` enables the MSW handlers in the dev server and e2e builds. */
  readonly VITE_MOCK_API?: string;
  /** `1` includes the dev component catalog in a production-style build. */
  readonly VITE_CATALOG?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
