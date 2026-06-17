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
  /**
   * Set to "0" to build a meetings-only shell: hides the activity-tracking
   * row (and its Accessibility splash step) and skips the engine auto-start.
   */
  readonly VITE_FEATURE_TRACKING?: string;
  /**
   * Set to "0" to hide the activity-tracking UI (the Capture card's status row
   * and Pause/Resume button) while still running the engine. Unlike
   * VITE_FEATURE_TRACKING=0, the capture engine keeps auto-starting and the
   * Accessibility splash step stays — only the visible surface is removed. Used
   * by headless-tracking brands that capture silently with no user toggle.
   */
  readonly VITE_FEATURE_TRACKING_UI?: string;
  /**
   * Set to "0" to request the Accessibility permission (FEATURE_TRACKING must
   * stay on, so the splash step shows) but NOT auto-start the capture engine.
   * Lets a release pre-grant Accessibility while deferring capture to a later
   * build that flips this back to "1" — no new permission prompt then. Default
   * on (capture auto-starts as usual).
   */
  readonly VITE_FEATURE_TRACKING_AUTOSTART?: string;
  /**
   * Set to "0" to hide the on-device transcription section in Settings.
   */
  readonly VITE_FEATURE_TRANSCRIPTION?: string;
  /**
   * Set to "0" to drop the Settings screen entirely: the footer gear is hidden
   * and the settings overlay is never opened. For shells that expose no
   * user-tunable settings.
   */
  readonly VITE_FEATURE_SETTINGS?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
