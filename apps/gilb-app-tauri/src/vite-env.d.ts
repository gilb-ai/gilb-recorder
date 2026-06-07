/// <reference types="vite/client" />

interface ImportMetaEnv {
  /**
   * Default gilb-web workspace URL prefilled in the Connect field. Baked in at
   * build time from the matching env file: `.env.development` (dev / `tauri dev`)
   * or `.env.production` (release / `tauri build`).
   */
  readonly VITE_GILB_WEB_URL?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
