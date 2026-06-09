/// <reference types="vite/client" />

interface ImportMetaEnv {
  /**
   * Default gilb-web workspace URL prefilled in the Connect field. Baked in at
   * build time from the matching env file: `.env.development` (dev / `tauri dev`)
   * or `.env.production` (release / `tauri build`).
   */
  readonly VITE_GILB_WEB_URL?: string;
  /**
   * UI locale baked in at build time: "en" (default) or "ru". Differently-
   * branded builds (see src/i18n.ts) pick their language here.
   */
  readonly VITE_LOCALE?: string;
  /**
   * Product name shown in user-facing strings ("Gilb" by default). Lets a
   * differently-branded build reuse this frontend without forking it.
   */
  readonly VITE_BRAND_NAME?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
