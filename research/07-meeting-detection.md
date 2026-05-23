# Meeting detection и screen recording: разбор rodnik-app и порт на Rust/Tauri

Источник: `reference/rodnik-app/src/features/meetingDetection/` и
`reference/rodnik-app/src/features/screenRecording/` (Electron-приложение
на Node.js). Этот документ — research-deliverable для карточки
[GILB-12](https://trello.com/c/IRr6gIVJ); он анализирует реализацию
rodnik'а и фиксирует, как её портировать на Rust/Tauri 2 для будущих
карточек [GILB-13] (детектор митингов macOS), [GILB-14] (детектор
митингов Windows), [GILB-15] (захват экрана + аудио), [GILB-16]
(транскрипция).

Все цитируемые пути относительны корня репо.

## 1. Signals (источники сигнала о начале / конце митинга)

### macOS — unified log + Control Center

`reference/rodnik-app/src/features/meetingDetection/macosLogStream.js`
запускает дочерний процесс:

```
log stream
    --type log
    --level default
    --predicate 'subsystem == "com.apple.controlcenter"
                 AND category == "sensor-indicators"
                 AND eventMessage BEGINSWITH "Active activity attributions changed to "'
    --style ndjson
```

`com.apple.controlcenter` — это системный демон, который отрисовывает
индикатор "оранжевая точка в menubar / Control Center" при доступе
приложений к микрофону. При смене состава активных индикаторов он пишет
в unified log строку вида:

```
Active activity attributions changed to ["mic:us.zoom.xos", "camera:com.apple.FaceTime"]
```

`parseMicAppsFromLogEvent` берёт JSON-массив из eventMessage, фильтрует
по префиксу `mic:`, отрезает префикс — получает bundle ID — и проверяет
через `isAllowlistedApp` из
`reference/rodnik-app/src/features/meetingDetection/allowlist.js`.
Сигнал дебаунсится 2000 ms (`DEBOUNCE_MS`) перед выдачей наверх — чтобы
схлопнуть быструю последовательность включился/выключился.

Логика "митинг начался / закончился" в
`reference/rodnik-app/src/features/meetingDetection/handlers/macosHandler.js`
сведена к count-based diff: `0 → N` = `handleMeetingDetected(apps)`,
`N → 0` = `handleMeetingEnded()`. Состав apps между этими переходами
игнорируется (это интенционально: митинг — это сам факт активного
микрофона, а не "Zoom специально").

**Достоинства подхода**: не требует elevated permission'ов
(`log stream` доступен обычному процессу), не требует accessibility,
работает в фоне.

**Ограничения**: предикат и формат `eventMessage` — это нестабильный
private interface Apple. Если Apple переименует subsystem или сменит
формат сообщения (как было с переходом от старых индикаторов к Control
Center), детектор молча перестанет работать. Сам факт того, что
сторонний процесс `log` всегда работает — это историческая deal-breaker
точка, но Apple её ещё не закрыли.

### Windows — WASAPI session events

`reference/rodnik-app/src/features/meetingDetection/windowsObserver.js`
поднимает Electron utility-process с нативным модулем
`mic_tracker.node`. Источник native-кода:
`reference/rodnik-app/native/mic-activity-tracker/src/session_events.cpp`
— реализует COM-интерфейс `IAudioSessionEvents` (методы
`OnStateChanged`, `OnSessionDisconnected`), полученный через
`IAudioSessionControl2` из WASAPI.

`reference/rodnik-app/src/features/meetingDetection/micMonitorProcess.js`
маршрутизирует события native-модуля по типу:

- `new_session` — сессия создана, но мик ещё не активен (логируется,
  countdown НЕ стартует).
- `session_active` — мик действительно зачитывает данные → добавляет в
  `activeMicApps`, диспатчит наверх.
- `session_inactive`, `session_expired` — снимает с `activeMicApps`.
- `default_device_changed` — игнорируется.

`reference/rodnik-app/src/features/meetingDetection/handlers/windowsHandler.js`
переходит в "митинг" по первому `session_active` (с пустым множеством)
и в "митинг закончен" когда `activeMicApps.size === 0` после
`session_inactive` или `session_expired`.

**Достоинства**: WASAPI отдаёт настоящий сигнал "приложение читает с
устройства захвата", не нужен парсинг логов. Маппинг
`process.exe → bundle ID` через
`WINDOWS_PROCESS_MAP`/`getBundleIdFromProcessName` в `allowlist.js`.

**Ограничения**: требует C++ native module и сборки под Windows, plus
утилитарного процесса (rodnik использует Electron `utilityProcess.fork`
с warmup 3s и backoff `[5s, 15s, 30s, 60s]`).

### Что НЕ используется как сигнал

- **Имена окон** не парсятся — rodnik умышленно отказался от текстовой
  эвристики (комментарии в `allowlist.js` помечают браузеры как
  "disabled — too many false positives from voice search, etc.").
- **Audio device API напрямую** на macOS не дёргается; полагаются на
  готовую агрегацию Control Center.
- **Процесс scan** (типа Get-Process) тоже не используется — слишком
  грубо, ловит "Zoom в трее, но не на звонке".

## 2. App list (как закодирован allowlist)

`reference/rodnik-app/src/features/meetingDetection/allowlist.js` —
единый источник правды. Две таблицы:

`MEETING_APP_ALLOWLIST` — bundle ID → `{ name }`:

- Video: Zoom (`us.zoom.xos`), Microsoft Teams (`com.microsoft.teams`,
  `com.microsoft.teams2` — две версии), Slack
  (`com.tinyspeck.slackmacgap`), FaceTime, Webex (`com.cisco.*` +
  `com.webex.*`), WhatsApp, Skype, Discord, VooV (Tencent), Tuple,
  Gather.
- Russian: Яндекс Телемост (`ru.yandex.desktop.telemost`),
  Контур.Толк (`kontur.talk`), SaluteJazz (`salutejazz.jazz-app`).
- Telephony: Aircall, Dialpad, Dialpad Meetings.
- AI: Perplexity Comet.

`WINDOWS_PROCESS_MAP` — `process.exe` (lowercased basename) → bundle ID.
Парсится через `path.basename(processName).toLowerCase()` в
`getBundleIdFromProcessName`. Имена: `zoom.exe`, `teams.exe`,
`ms-teams.exe`, `webex.exe`, `voovmeetingapp.exe`, `tuple.exe`,
`gather.exe`, `yandextelemost.exe`, `jazz.exe`, `ktalk.exe`, `slack.exe`,
`discord.exe`, `whatsapp.exe`, `skype.exe`, `aircall.exe`,
`aircall workspace.exe`, `dialpad.exe`, `dialpadmeetings.exe`,
`comet.exe`.

Браузеры (Chrome, Firefox, Safari, Edge, Brave, Arc, Vivaldi, Zen,
Yandex Browser) **закомментированы** в обеих таблицах. Решение: голос в
браузере (voice search, видеоплеер с unmute, WebRTC-демо) даёт слишком
много ложных срабатываний.

**Для gilb**: возьмём ту же структуру (статический map в коде, не из
БД и не из конфига). Российские приложения сразу включены. Браузеры
оставляем закомментированными. Bundle ID — канонический ключ (Windows
маппится в него же), это упрощает дедуп и UI ("в митинге участвует
Zoom" — единое имя на обеих платформах).

## 3. Pipeline (захват видео + аудио, файлы, lifecycle)

`reference/rodnik-app/src/features/screenRecording/screenRecordingService.js`
+ `reference/rodnik-app/src/ui/screenRecording/recorder.js` —
двухуровневая конструкция:

### Захват выполняется в renderer-процессе

Rodnik открывает скрытое Electron-окно и в нём вызывает Web API:

- `navigator.mediaDevices.getDisplayMedia({ video: { displaySurface: 'monitor' }, audio: true })`
  — экран + system audio (system audio опционален, может отсутствовать).
- `navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true }, video: false })`
  — микрофон (обязателен; если нет — error и отмена).

Аудио миксуется в `AudioContext`: `MediaStreamDestination` принимает
два source-узла (mic + опциональный system audio), результат — единый
audio track.

### Два параллельных MediaRecorder

Видео + смешанное аудио:
- mimeType — best-of-list: `video/webm;codecs=vp9,opus` →
  `video/webm;codecs=vp8,opus` → `video/webm;codecs=vp9` →
  `video/webm;codecs=vp8` → `video/webm`.
- `videoBitsPerSecond: 2500000` (2.5 Mbps).
- Chunks каждые 1000 ms (`.start(1000)`), отправляются IPC в main.

Только аудио (для транскрипции):
- mimeType — `audio/webm;codecs=opus` (если поддерживается).
- `audioBitsPerSecond: 64000` (64 kbps).
- Chunks каждые 1000 ms.

### Дисковый layout

`reference/rodnik-app/src/features/screenRecording/screenRecordingService.js`:

- Корень: `app.getPath('userData')/recordings/`.
- Имя сессии: `rodnik-YYYY-MM-DD_HH-MM-SS` (datetime ISO с заменой
  `T`→`_`, `:`→`-`).
- Два файла на сессию:
  - `<basePath>.webm` — видео + аудио.
  - `<basePath>.audio.webm` — только аудио.

Write streams открываются на старте, chunks из renderer'а через IPC
дозаписываются (`writeStream.write(buf)`), закрываются на стопе с
ожиданием `finish`-event'а.

### Lifecycle и ограничения

- **MAX_DURATION_LIMIT_MS = 2 * 60 * 60 * 1000** — 2 часа жёсткий
  таймер; по истечении `stopRecording('duration_limit')`.
- **MIN_DURATION_MS = 10_000** — записи короче 10 секунд удаляются с
  диска (`_deleteFiles`), в upload-очередь не идут.
- На старте recording'а `uploadQueueService.pause()` — чтобы аплоад
  чужих записей не отбирал bandwidth у текущей сессии.
- На стопе — `uploadQueueService.enqueue(basePath, meta)` и
  `uploadQueueService.resume()`.
- Стоп ждёт подтверждения от renderer'а до 5 секунд (`Stop timeout`),
  потом форсит cleanup.
- Force-stop, ошибки и обрыв `displayStream` (пользователь закрыл
  share-диалог) маршрутизируются в общий `_finalizeRecording`.

### Post-processing (НЕ применяется онлайн в рабочем коде)

`reference/rodnik-app/src/features/screenRecording/webmMetadataFixer.js`
существует и использует `webm-duration-fix-buffer` для добавления
duration + Cues (seek index) в готовый .webm. MediaRecorder пишет WebM
без правильной длительности и без index'а — ремукс через эту библиотеку
делает файл seekable. В `screenRecordingService.js` он сейчас НЕ
вызывается; ремукс делается на сервере при upload или плеером ad-hoc.

## 4. Rust crate mapping (что чем закрываем)

### macOS log stream — отдельный процесс или подписка через OSLog?

Прямой эквивалент `log stream` — это `os_log_stream` (XPC через
`com.apple.diagnosticd`). Готовых high-quality Rust-обёрток нет. Два
варианта:

1. **Простой и совместимый с rodnik**: спавнить `/usr/bin/log` через
   `tokio::process::Command`, читать NDJSON со stdout, парсить
   `serde_json`. Сохраняет точно ту же поверхность сигнала, ровно тот
   же предикат — минимизирует риск расхождения. Cost: один extra
   процесс ~постоянно, но это уже факт жизни rodnik'а.
2. **Более "Rusty"**: написать тонкий Swift-bridge через `swift-rs`
   или Objective-C bridge через `objc2` поверх приватного API
   `OSLogStore` / `os_log_create_log_store`. Это сэкономит fork
   `log`-процесса, но `OSLogStore` — приватный API; review при
   нотаризации не упадёт, но это потенциальная mine.

**Рекомендация: вариант 1.** Стоит написать `gilb-meeting` так, чтобы
implementation сидела за trait и могла быть заменена в будущем.

Crates:
- `tokio = { version = "1", features = ["process", "io-util"] }` (уже
  в workspace).
- `serde = { features = ["derive"] }` + `serde_json` (уже в
  workspace) — парсить NDJSON.

### Windows audio session events

`session_events.cpp` в rodnik'е реализует `IAudioSessionEvents` через
COM. На Rust:

- **`windows` crate** (Microsoft official, `windows = "0.58"` или
  свежее) с features `Win32_Media_Audio`, `Win32_System_Com`,
  `Win32_Media_Audio_Endpoints`. Даёт `IMMDeviceEnumerator`,
  `IAudioSessionManager2`, `IAudioSessionControl2`,
  `IAudioSessionEvents`. Это direct port — те же COM-интерфейсы, теми
  же именами.

- Альтернатива — `cpal` (cross-platform audio). НЕ подходит: `cpal`
  для actual playback/capture, а не для observe-only session events.
  Используем `windows` напрямую.

Маппинг `process.exe → bundle ID` достижим через
`IAudioSessionControl2::GetProcessId` → `OpenProcess` →
`QueryFullProcessImageNameW` → basename + lowercase →
`WINDOWS_PROCESS_MAP`. Всё это есть в том же `windows` crate (features
`Win32_System_ProcessStatus`, `Win32_System_Threading`).

### Screen + audio capture (Layer-15)

`MediaRecorder` в renderer'е — это **не то, что мы хотим в gilb'е**.
Tauri 2 умеет открывать renderer-окна и звать WebView2/WKWebView APIs,
но качество и контроль ниже, чем у native pipeline. Варианты:

**macOS — ScreenCaptureKit + AVFoundation:**

- `screencapturekit-rs` (Rust bindings к ScreenCaptureKit). Даёт
  захват displays/windows, system audio, callback по фреймам в
  CMSampleBuffer. Минус: библиотека молодая, API ещё движется. Цена
  замены — переписать тонкий слой захвата.
- Альтернатива — `cidre` (Pavel Tafintsev's bindings) или ручной
  Swift-bridge через `swift-rs`.
- Для mic: `cpal` с CoreAudio backend (уже популярен в Rust audio
  экосистеме), или прямой `coreaudio-rs`. `cpal` — кросс-платформенный
  → один и тот же код для Windows.
- Энкодинг: `ScreenCaptureKit` уже отдаёт CMSampleBuffer; запись через
  `AVAssetWriter` (`objc2`/Swift-bridge) даст native .mov/.mp4 с
  H.264/HEVC и AAC — без вмешательства user-space энкодера.

**Windows — Graphics Capture + WASAPI:**

- Windows.Graphics.Capture через `windows` crate (features
  `Graphics_Capture`, `Graphics_DirectX_Direct3D11`). Даёт желаемый
  per-monitor / per-window захват с честной поддержкой Win10+.
- Mic через `cpal` (WASAPI backend).
- Энкодинг: Media Foundation H.264/H.265 + AAC через `windows` crate
  (`Media_Transcoding`), или вызов `ffmpeg-sys` — но FFmpeg как
  dependency лучше держать вне ядра (лицензия GPL/LGPL, размер).

**Кросс-платформенно (если важна минимальная разница):**

- `cpal` для микрофона на обеих платформах — единый интерфейс.
- Энкодинг ОПТИМАЛЬНО оставить native (ScreenCaptureKit / Media
  Foundation), потому что они аппаратно ускорены и не требуют
  лицензии на софтовый кодек.

### Что нужно на Swift-bridge

- macOS Screen capture с ScreenCaptureKit и AVAssetWriter, если
  `screencapturekit-rs` окажется слишком ограниченной для нашей
  кадровой частоты / audio sync. Тогда — отдельный Swift-таргет в
  `apps/gilb-app-tauri/src-tauri/swift/`, FFI через `swift-rs`.
- Запрос пермишена на System Audio Capture (новинка macOS 13+):
  `SCContentSharingPicker`/`SCStreamConfiguration.capturesAudio = true`.

## 5. Recommended event surface (что должен экспонировать `gilb-meeting`)

Crate `gilb-meeting` (новый, добавляется в Cargo workspace, как сейчас
добавлены `gilb-a11y`, `gilb-engine`). Идея — пассивный наблюдатель,
который кладёт события в `EventBus` (`gilb-events`), как сейчас делает
`gilb-a11y` с permission/health events.

```rust
// gilb-meeting/src/lib.rs
#[derive(Clone, Debug)]
pub enum MeetingEvent {
    Started {
        at: chrono::DateTime<chrono::Utc>,
        apps: Vec<MeetingApp>,
    },
    AppsChanged {
        at: chrono::DateTime<chrono::Utc>,
        apps: Vec<MeetingApp>,
    },
    Ended {
        at: chrono::DateTime<chrono::Utc>,
        duration: std::time::Duration,
    },
    HealthDegraded {
        reason: String,  // "log stream closed unexpectedly", etc.
    },
}

#[derive(Clone, Debug)]
pub struct MeetingApp {
    pub bundle_id: String,   // канонический ключ
    pub display_name: String,
}

#[async_trait::async_trait]
pub trait MeetingDetector: Send + Sync {
    async fn start(&self, bus: gilb_events::EventBus) -> anyhow::Result<()>;
    async fn stop(&self) -> anyhow::Result<()>;
}
```

Реализации:

- `MacosLogDetector` (cfg-gated `target_os = "macos"`) — спавнит
  `log stream`, парсит NDJSON, debounce 2000 ms.
- `WindowsAudioSessionDetector` (cfg-gated `target_os = "windows"`) —
  владелец COM apartment-thread'а с `IAudioSessionEvents`.
- `NoopDetector` для unsupported platforms.

Что **НЕ** делает `gilb-meeting`:

- Не управляет UI (countdown / "meeting ended" windows). У rodnik'а
  state machine и UI вплавлены в `meetingDetectionService.js` — нам не
  нужно, gilb пишет всегда (Layer 1 = "сбор сырых a11y-данных", см.
  `CLAUDE.md`). State machine с `IDLE/COUNTDOWN/RECORDING/ENDING/...`
  переносится на UI-уровень gilb (Tauri commands в
  `apps/gilb-app-tauri/src-tauri/src/lib.rs`), если/когда мы вообще
  захотим countdown — отдельная UX-карточка.
- Не пишет на диск, не аплоадит, не транскрибирует. Это всё для
  отдельных crate'ов (Layer-15 = capture-pipeline, Layer-16 =
  transcribe).
- Не дублирует `EventBus` — переиспользует `gilb_events::EventBus`,
  чтобы permission-, health- и meeting-события шли по одной шине.

## 6. Audio format contract (что отдаём в [GILB-16])

Транскрипция (Whisper, Apple SpeechAnalyzer, что бы мы ни выбрали) —
самый дорогой шаг. Цель аудио-формата: **никаких лишних перекодировок**
между захватом и подачей в транскрайбер.

**Контракт**:

- Формат файла: **WAV (PCM)**, 16 kHz, mono, 16-bit signed
  little-endian. Это формат, который любит ВСЁ: whisper.cpp,
  faster-whisper, Apple SpeechAnalyzer, AssemblyAI и Deepgram (оба
  без `Content-Type: audio/wav` headers тоже принимают). 16 kHz mono —
  стандартный input для речевых моделей; никакой апсемплинг не нужен.
- Channel layout: **mono из mic + system audio**, смешанные в
  один канал. Rodnik делает то же самое через `AudioContext` mix.
- Файл: `<basePath>.wav` рядом с видео, либо отдельная сессия в
  `~/.gilb/recordings/<session>/audio.wav`.
- Дополнительно: видео в **`.mov` / `.mp4` (H.264/HEVC + AAC)** на
  macOS и **`.mp4` (H.264 + AAC)** на Windows — это даёт seekable
  файл без post-fix'а (в отличие от `.webm` у rodnik'а, который
  требует `webmMetadataFixer.js`).

**Почему не Opus**: rodnik'овские 64 kbps Opus отлично подходят для
аплоада, но whisper.cpp требует декода в PCM перед инференсом. Если
gilb транскрибирует **локально** (а это базовое предположение для
desktop-app с акцентом на privacy), Opus — лишний код-туда-сюда. Если
будем ещё и аплоадить — да, второй файл в Opus имеет смысл, но MVP без
Server Side не делает аплоад.

**Реализация**:

- На macOS — `AVAudioRecorder` или `AVAssetReader` over
  ScreenCaptureKit audio stream, конфиг 16 kHz / mono / PCM. Альтернатива
  — `cpal` пишет PCM в файл напрямую (через `hound` crate для WAV
  header'а).
- На Windows — WASAPI capture через `cpal`, тот же `hound` для WAV.
- Внутри `gilb-capture` (Layer-15) контракт: после `stop_recording`
  на диске гарантированно лежит `audio.wav` 16 kHz mono PCM. Эту
  гарантию документируем в crate-level doc.

## 7. Sub-task validation (что должно сходиться с этим документом)

Дальнейшие карточки и как они должны "защищать" решения отсюда:

**[GILB-13] macOS meeting detector**:
- Спавнит `/usr/bin/log` с предикатом из §1.
- Использует `MEETING_APP_ALLOWLIST` (тот же набор bundle ID, что в
  `reference/rodnik-app/src/features/meetingDetection/allowlist.js`,
  включая russian apps и без браузеров).
- Debounce 2000 ms.
- Эмитит `MeetingEvent::Started / Ended / AppsChanged` в
  `EventBus`.
- Test: unit-тест на парсинг строки
  `Active activity attributions changed to ["mic:us.zoom.xos"]`
  (можно копипастой с `reference/rodnik-app/src/features/meetingDetection/__tests__/parser.test.js`).

**[GILB-14] Windows meeting detector**:
- COM apartment-thread с `IAudioSessionEvents`.
- Маршрутизация событий 1-в-1 с
  `reference/rodnik-app/src/features/meetingDetection/micMonitorProcess.js`
  (`session_active`/`session_inactive`/`session_expired`/`new_session`).
- Backoff на restart: `[5s, 15s, 30s, 60s]` warmup 3s — точные числа
  из `windowsObserver.js`.
- Маппинг `process.exe → bundle ID` через `WINDOWS_PROCESS_MAP`.

**[GILB-15] Screen + audio capture**:
- Native pipeline: ScreenCaptureKit + AVAssetWriter на macOS,
  Windows.Graphics.Capture + Media Foundation на Windows.
- НЕ копирует rodnik'овский Electron-MediaRecorder подход (это
  legacy renderer-process trick, не fit для нашей Rust-архитектуры).
- Файлы: `<session>/video.mov` (или `.mp4`) + `<session>/audio.wav`
  (16 kHz mono PCM, см. §6).
- Лимиты как у rodnik: 2 часа max, 10 секунд min, иначе удаляем.
- Подписка на `MeetingEvent::Started` / `Ended` для auto-start /
  auto-stop — но автозапуск выключен по умолчанию (как у rodnik'а
  через `meetingAutoDetection` setting в electron-store).

**[GILB-16] Transcription**:
- Принимает `<session>/audio.wav`, отдаёт `<session>/transcript.json`.
- Контракт формата файла — §6. Если транскрайбер хочет другой sample
  rate, ресемплинг — его (транскрайбера) забота, не ours.

**Невалидация (failure modes, которые надо явно ловить)**:

- macOS: `log stream` падает / закрылся. У rodnik'а через `onError`
  callback — мы шлём `MeetingEvent::HealthDegraded` и ретраим с
  backoff'ом.
- Windows: native COM-thread падает. Аналогично — `HealthDegraded` +
  backoff.
- ScreenCaptureKit отказали в permission'е → fail-fast при
  `start_recording`, ошибка пробрасывается в Tauri command.
- Микрофон занят другим процессом эксклюзивно (редкий случай на
  Windows pre-WASAPI shared mode) → лог + продолжение с системным
  аудио.

## Заключение

**Рекомендация: YES, портируем — но не один-в-один.**

- Логику детекции митингов (§1, §2) переносим напрямую: тот же
  unified-log predicate на macOS, те же WASAPI session events на
  Windows, тот же allowlist. Это уже отлажено rodnik'ом на пользователях,
  риск переписать с нуля выше, чем риск порта.
- State machine, countdown windows, auto-start/stop UI — **не
  переносим в Layer 1**. У gilb'а другая модель: запись либо ведётся
  всегда (sliding buffer), либо запускается явно. Meeting-events —
  богатые метки в общем event-stream'е, а не триггер записи.
- Pipeline записи (§3) **переписываем native'но**: Electron's
  MediaRecorder в renderer-окне — это рабочее, но архитектурно
  чужеродное для Tauri+Rust решение. Native ScreenCaptureKit и
  Windows.Graphics.Capture дают лучшее качество, аппаратное ускорение,
  и не требуют `webmMetadataFixer.js`-style post-processing.
- Аудио-контракт (§6) — 16 kHz mono WAV — фиксируется сразу, чтобы
  [GILB-16] не диктовал нам формат потом.

Главный риск: macOS unified-log приватный интерфейс. Mitigation —
trait-abstraction в `gilb-meeting`, чтобы при поломке Apple'ом мы могли
заменить implementation (на `OSLogStore` Swift-bridge, на `AVAudioEngine`
input-tap approach, или на честный device-enumeration heuristic — все
три варианта research'ились).
