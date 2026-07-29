// Build-time i18n + branding. The locale and product name are baked in per
// build via Vite env (`VITE_LOCALE`, `VITE_BRAND_NAME`), so one frontend
// serves differently-branded builds (e.g. an English "Gilb" and a Russian
// build under another name) without forking — same mechanism as
// `VITE_GILB_WEB_URL`.
//
// Static markup is translated declaratively: elements carry
// `data-i18n="key"` and `applyI18n()` (called once per entry point on
// DOMContentLoaded) replaces their text content. Dynamic strings go through
// `t(key, params)`; `{placeholders}` are substituted from `params`, and
// `{brand}` is always available.

export const BRAND = import.meta.env.VITE_BRAND_NAME ?? "Gilb";

const LOCALE = import.meta.env.VITE_LOCALE === "ru" ? "ru" : "en";

const en = {
  "splash.title": "macOS permissions needed",
  "splash.accessibility": "Accessibility",
  "splash.screenRecording": "Screen Recording",
  "splash.microphone": "Microphone",
  "splash.checking": "checking…",
  "splash.openAccessibility": "Open Accessibility",
  "splash.openScreenRecording": "Open Screen Recording",
  "splash.openMicrophone": "Open Microphone",
  "splash.hint":
    "The window updates automatically once you grant each toggle. If the status doesn't change after granting, restart the app.",
  "splash.granted": "granted",
  "splash.notGranted": "not granted",

  "settings.title": "Settings",
  "settings.meetingLabel": "Enable meeting detection",
  "settings.meetingDesc": "We'll catch the start of every call and offer to record.",
  "settings.transcription": "Transcription",
  "settings.transcriptionDesc":
    "Runs on this Mac — audio never leaves your device. Transcription turns on automatically once the model is downloaded.",
  "settings.language": "Language",
  "settings.langAuto": "Auto-detect",
  "settings.langRu": "Russian",
  "settings.langEn": "English",
  "settings.cancel": "Cancel",
  "settings.save": "Save",

  "model.notDownloaded": "Not downloaded",
  "model.ready": "Ready",
  "model.download": "Download",
  "model.cancel": "Cancel",
  "model.delete": "Delete",
  "model.retry": "Retry",
  "model.downloading": "Downloading…",
  "model.downloadingPct": "Downloading… {pct}%",
  "model.failed": "Download failed",
  "model.failedWith": "Download failed — {error}",

  "capture.title": "Capture",
  "capture.recordingMeeting": "Recording meeting — ",
  "capture.recordingManual": "Recording screen",
  "capture.thisMeeting": "this meeting",
  "capture.stop": "Stop",

  "assist.title": "{brand} Assist",
  "assist.empty": "Suggestions will appear here during the meeting.",
  "assist.thinking": "Thinking…",
  "assist.askPlaceholder": "Ask about the conversation… (Enter)",
  "assist.hide": "Hide",
  "assist.error": "Assist error: {error}",

  // The switch in the signed-in workspace card. The first turn-on downloads
  // the speech model, so the off state says so up front.
  "assist.label": "Live suggestions",
  "assist.descOn": "During a meeting, suggestions appear over the other windows. ⌘\\ shows and hides them.",
  "assist.descOff": "Off — no suggestions during meetings.",
  "assist.descNeedsModel": "First turn-on downloads the speech recognition model (~570 MB).",
  "assist.descDownloading": "Downloading the model… {pct}%",
  "capture.trackingLabel": "Activity tracking",
  "capture.trackingOn": "Activity tracking — on",
  "capture.trackingPaused": "Activity tracking — paused",
  "capture.pause": "Pause",
  "capture.resume": "Resume",
  "capture.trackingPausedMsg": "Activity tracking paused",
  "capture.trackingOnMsg": "Activity tracking on",
  "capture.cantResume": "Couldn't resume tracking: {error}",
  "capture.cantPause": "Couldn't pause tracking: {error}",
  "capture.stopError": "stop recording error: {error}",
  "capture.statusError": "status error: {error}",

  "auth.lead": "Connect this recorder to your {brand} workspace to enable Enterprise features.",
  "auth.connect": "Connect",
  "auth.connectedLine": "Connected to your {brand} workspace",
  "auth.signOut": "Sign out",
  "auth.thisDevice": "this device",
  "auth.continueInBrowser": "Continue sign-in in your browser…",
  "auth.signInError": "sign-in error: {error}",
  "auth.signedOut": "Signed out",
  "auth.signOutError": "sign-out error: {error}",
  "auth.signInFailed": "Sign-in failed",

  "update.installing": "Installing update {version}…",
  "app.version": "{brand} v{version}",

  "countdown.startLead": "{brand} Meeting Recording is about to start for",
  "countdown.stopLead": "{brand} Meeting Recording is about to stop for",
  "countdown.record": "Record now",
  "countdown.cancel": "Cancel",
  "countdown.stopNow": "Stop now",
  "countdown.keep": "Keep recording",
};

const ru: typeof en = {
  "splash.title": "Нужны разрешения macOS",
  "splash.accessibility": "Универсальный доступ",
  "splash.screenRecording": "Запись экрана",
  "splash.microphone": "Микрофон",
  "splash.checking": "проверка…",
  "splash.openAccessibility": "Открыть «Универсальный доступ»",
  "splash.openScreenRecording": "Открыть «Запись экрана»",
  "splash.openMicrophone": "Открыть «Микрофон»",
  "splash.hint":
    "Окно обновится автоматически после выдачи каждого разрешения. Если статус не меняется, перезапустите приложение.",
  "splash.granted": "разрешено",
  "splash.notGranted": "запрещено",

  "settings.title": "Настройки",
  "settings.meetingLabel": "Автодетекция встреч",
  "settings.meetingDesc": "Заметим начало каждого звонка и предложим записать.",
  "settings.transcription": "Транскрипция",
  "settings.transcriptionDesc":
    "Работает на этом Mac — аудио не покидает устройство. Транскрипция включится автоматически после загрузки модели.",
  "settings.language": "Язык",
  "settings.langAuto": "Автоопределение",
  "settings.langRu": "Русский",
  "settings.langEn": "Английский",
  "settings.cancel": "Отмена",
  "settings.save": "Сохранить",

  "model.notDownloaded": "Не загружена",
  "model.ready": "Готова",
  "model.download": "Загрузить",
  "model.cancel": "Отмена",
  "model.delete": "Удалить",
  "model.retry": "Повторить",
  "model.downloading": "Загрузка…",
  "model.downloadingPct": "Загрузка… {pct}%",
  "model.failed": "Ошибка загрузки",
  "model.failedWith": "Ошибка загрузки — {error}",

  "capture.title": "Запись",
  "capture.recordingMeeting": "Идёт запись встречи — ",
  "capture.recordingManual": "Идёт запись экрана",
  "capture.thisMeeting": "эта встреча",
  "capture.stop": "Остановить",

  "assist.title": "{brand} — подсказки",
  "assist.empty": "Подсказки появятся здесь во время встречи.",
  "assist.thinking": "Думаю…",
  "assist.askPlaceholder": "Спросить о разговоре… (Enter)",
  "assist.hide": "Скрыть",
  "assist.error": "Ошибка подсказок: {error}",

  "assist.label": "Онлайн-подсказки",
  "assist.descOn": "Во время встречи подсказки показываются поверх других окон. ⌘\\ — показать или скрыть.",
  "assist.descOff": "Выключены — во время встречи подсказок не будет.",
  "assist.descNeedsModel": "При первом включении скачается модель распознавания речи (~570 МБ).",
  "assist.descDownloading": "Загрузка модели… {pct}%",
  "capture.trackingLabel": "Отслеживание активности",
  "capture.trackingOn": "Отслеживание активности — включено",
  "capture.trackingPaused": "Отслеживание активности — на паузе",
  "capture.pause": "Пауза",
  "capture.resume": "Возобновить",
  "capture.trackingPausedMsg": "Отслеживание активности на паузе",
  "capture.trackingOnMsg": "Отслеживание активности включено",
  "capture.cantResume": "Не удалось возобновить отслеживание: {error}",
  "capture.cantPause": "Не удалось приостановить отслеживание: {error}",
  "capture.stopError": "Ошибка остановки записи: {error}",
  "capture.statusError": "Ошибка статуса: {error}",

  "auth.lead": "Подключите рекордер к рабочему пространству {brand}, чтобы записи загружались в облако.",
  "auth.connect": "Войти в аккаунт",
  "auth.connectedLine": "Подключено к рабочему пространству {brand}",
  "auth.signOut": "Выйти из аккаунта",
  "auth.thisDevice": "это устройство",
  "auth.continueInBrowser": "Продолжите вход в браузере…",
  "auth.signInError": "Ошибка входа: {error}",
  "auth.signedOut": "Вы вышли из аккаунта",
  "auth.signOutError": "Ошибка выхода: {error}",
  "auth.signInFailed": "Не удалось войти",

  "update.installing": "Установка обновления {version}…",
  "app.version": "{brand} v{version}",

  "countdown.startLead": "{brand}: сейчас начнётся запись встречи",
  "countdown.stopLead": "{brand}: сейчас остановится запись встречи",
  "countdown.record": "Записать",
  "countdown.cancel": "Отмена",
  "countdown.stopNow": "Остановить запись",
  "countdown.keep": "Продолжить запись",
};

export type MessageKey = keyof typeof en;

const STRINGS: Record<MessageKey, string> = LOCALE === "ru" ? ru : en;

/** Translate `key`, substituting `{placeholders}` from `params` (+ `{brand}`). */
export function t(key: MessageKey, params?: Record<string, string | number>): string {
  const s = STRINGS[key] ?? key;
  const all: Record<string, string | number> = { brand: BRAND, ...params };
  // Single pass: a substituted value that itself contains `{...}` (e.g. a
  // backend error echoed into a message) must never be re-substituted.
  return s.replace(/\{(\w+)\}/g, (match, name) =>
    name in all ? String(all[name]) : match,
  );
}

/**
 * Replace the text of every `[data-i18n="key"]` element with its translation
 * and set the window title to the brand name. Call once on DOMContentLoaded;
 * markup keeps its English text as the fallback for missing keys.
 */
export function applyI18n(): void {
  document.documentElement.lang = LOCALE;
  document.title = BRAND;
  for (const el of document.querySelectorAll<HTMLElement>("[data-i18n]")) {
    const key = el.dataset.i18n as MessageKey | undefined;
    if (key && STRINGS[key] !== undefined) el.textContent = t(key);
  }
}
