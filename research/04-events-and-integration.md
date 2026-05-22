# Events, Tauri ↔ Rust, HTTP API, pipes, redact, config

Crates:
- `/Users/leonid/src/gilb/reference/screenpipe/crates/screenpipe-events/`
- `/Users/leonid/src/gilb/reference/screenpipe/crates/screenpipe-redact/`
- `/Users/leonid/src/gilb/reference/screenpipe/crates/screenpipe-config/`
- `/Users/leonid/src/gilb/reference/screenpipe/apps/screenpipe-app-tauri/`

## 1. Event bus (screenpipe-events)

`events_manager.rs:12-33` — singleton поверх `tokio::sync::broadcast`.

```rust
static EVENT_MANAGER: Lazy<EventManager> = …;
tokio::sync::broadcast::Sender<Event>  // capacity 10 000

pub async fn send_event<T: Serialize>(name: &str, data: T);
pub fn subscribe_to_event<T>(name: &str) -> EventSubscription<T>;
pub fn subscribe_to_all_events() -> EventSubscription<serde_json::Value>;
```

**Cleanup**: фоновый task раз в 60 с удаляет subscriptions, не читавшиеся
10 минут. `RwLock` для thread-safe tracking.

### Типы кастомных событий (`custom_events/`)

| Event | Поля | Когда |
|-------|------|-------|
| `WorkflowEvent` | event_type, confidence (0..1), activities[] | детектор паттернов нашёл что-то |
| `PermissionEvent` | kind (ScreenRecording/Microphone/Accessibility/Keychain), state (Lost/Restored), reason | TCC лишился разрешения |
| `PipeCompletedEvent` | pipe_name, success, duration_secs | агент закончил работу |
| `MeetingEvent` | app, ts, calendar_title, attendees | meeting_started/ended |

События публикуются из capture pipeline (OCR / transcription / UI frame /
realtime_transcription), потребляются meeting detector'ом и пользовательскими
pipes.

## 2. Tauri ↔ Rust core

Архитектура — два долгожителя:

```
ServerCore (живёт весь app):
  • DB (Arc<DatabaseManager>)
  • HTTP server (axum, по умолчанию :3030)
  • PipeManager (агенты)
  • PowerManager
  • Redaction shutdown signal

CaptureSession (toggleable, start/stop):
  • VisionManager
  • AudioManager
  • UiRecorder (screenpipe-a11y)
  • MeetingWatcher
```

**IPC**: `#[tauri::command]` (sync/async RPC), TypeScript bindings через
`#[specta::specta]` в debug builds. Примеры: `vault_status`, `vault_unlock`,
`get_env`, `get_e2e_seed_flags`, `set_window_size`, `handle_focus`.

**Init flow** (`server_core.rs:60-120`):
1. `FD_LIMIT=8192`, `HF_ENDPOINT` (для китайских mirror'ов).
2. Analytics init (opt-in).
3. DB init с retry на lock — `DB_LOCK_RETRY_DELAYS_SECS: [0, 2, 5]`.
4. Audio manager + HTTP server bind.
5. Pipe manager.
6. Background workers (power monitor, sleep monitor, redaction worker).

**`local_api_key: Option<String>`** — frontend получает его через Tauri command,
инжектирует в headers HTTP-запросов (localFetch).

## 3. HTTP API (axum)

`server.rs`:

- `POST /notifications`, `GET /notifications`, `DELETE /notifications/{id}`
- `POST /inbox` — отправка inbox сообщения
- `POST /log` — pipe logging
- `POST /auth` — token / email / user_id
- `GET /app-icon` — icon серверу
- `POST /window-size`
- `POST /focus` — deep links / navigation

Middleware: `tower-http::CorsLayer` + `TraceLayer`. Notifications server
отдельно — port 11435.

## 4. Pipes (плагин-система ~ AI агенты)

`screenpipe-core/src/pipes/mod.rs:1-100`.

Pipe = директория `~/.screenpipe/pipes/{name}/` с `pipe.md`:
YAML frontmatter + markdown prompt.

**PipeConfig** (`pipes/mod.rs:70-162`):

```yaml
schedule: "0 */2 * * *"          # или "every 30m" / "daily" / "manual"
enabled: true
agent: "pi"                       # "pi" | "claude-code" | "opencode"
model: "claude-haiku-4-5"
provider: openai                  # опционально override
preset: ["..."]                   # IDs из store.bin
connections: ["obsidian", "slack"]
trigger:
  events: ["workflow_event"]      # subscribe в event bus
  custom: ["..."]                 # future: embedding triggers
```

**Data access control** (README:169-176):
- `allow-apps`, `deny-apps`, `deny-windows` (glob)
- Content type: `ocr | audio | input | accessibility`
- `time-range`, `days`
- `allow-raw-sql`, `allow-frames`
- 3 уровня enforcement: skill gating + agent interception + server middleware
  (cryptographic per-pipe tokens).

**Executors** (`agents/mod.rs:40-66`):
trait `AgentExecutor` с `run()` / `run_streaming()`; impls — `PiExecutor`,
`OpenCodeExecutor`. Поддерживает `continue_session`, `shared_pid`,
provider/url/api_key override.

PiExecutor устанавливает extensions: filtered permissions, context pruning,
orphan guard, subagent composition.

## 5. Redact (PII)

`screenpipe-redact/src/lib.rs:1-70`.

**Background worker** (не блокирует capture):

```rust
WorkerConfig {
    batch_size: 32,
    idle_between_batches: 50ms,
    poll_interval: 5s,
}
```

Обрабатывает **newest-first** (поиск чаще на свежих данных). Destructive:
переписывает source column, без sibling-копии.

**Trait `Redactor`** (`lib.rs:103-134`):

```rust
fn redact(&str) -> RedactionOutput;
fn redact_batch(&[String]) -> Vec<RedactionOutput>;
```

Impls: `RegexRedactor`, `TinfoilRedactor`, `OnnxRedactor`, `RfDetrRedactor`.

**Pipeline** (`pipeline.rs`):
1. Regex pass (детерминированный, free).
2. Cache hit на (text, regex_version) tuple.
3. AI fallback (Tinfoil/ONNX) для residual.
4. **Fails closed** — на ошибке возвращаем regex output (никогда unredacted).

**Regex patterns** (`adapters/regex.rs:44-100`):
private key markers, connection strings `postgres://user:pass@host`,
API key prefixes (sk-, ghp_, xoxb-, AKIA, ya29., hf_), JWT (eyJ…),
email/phone (с разделителями — против false positives), SSN, CC (с Luhn), IPv4.

**Image redaction** — RFDETR-Nano ONNX, solid black rectangles (не blur —
тот reversible), atomic JPG overwrite + timestamp stamp.

Target tables: `ocr_text`, `audio_transcriptions`, `accessibility`,
`ui_events` (keyboard/clipboard).

**Tinfoil**: AMD SEV-SNP attestation + Sigstore + TLS pinning; LRU cache
SHA256 (2000 / 1 ч); batch до 8 concurrent.

## 6. Конфигурация

`screenpipe-config/src/recording.rs` — `RecordingSettings` shared между
desktop, CLI, engine. camelCase serde rename для совместимости с
`store.bin` (Tauri).

Поля включают: `disableAudio`, `audioTranscriptionEngine`, `transcriptionMode`,
`meetingLiveTranscriptionEnabled`, `meetingLiveTranscriptionProvider`,
`audioDevices`, `followSystemAudioDevices`, `vocab: [(word, replacement)]`,
`scheduleRules: ScheduleRule[]`.

**Persistence** (`persistence.rs`):
- CLI: TOML `~/.screenpipe/config.toml`, default если нет.
- Desktop: `store.bin` через `tauri-plugin-store`.

**Env vars**:
- `SCREENPIPE_DATA_DIR` — override `~/.screenpipe`
- `SCREENPIPE_FD_LIMIT=8192`
- `SCREENPIPE_ANALYTICS_ID`
- `HF_ENDPOINT` (Chinese mirror)
- `SCREENPIPE_E2E_SEED`

## 7. Storage layout

```
~/.screenpipe/             ($SCREENPIPE_DATA_DIR override)
├── db.sqlite
├── data/                  snapshots, video, audio
├── config.toml
├── pipes/{name}/pipe.md
├── logs/                  RollingFileAppender, daily rotation
└── secrets/               screenpipe-secrets (keychain)
```

## 8. Logging / observability

`tracing` + `tracing_appender::rolling(DAILY)` + `tracing_subscriber::EnvFilter`
+ Sentry integration. На macOS дополнительно `tracing_oslog::OsLogger` (отдаёт
в `os_log`).

Health monitoring (`health.rs`): boot phases, stall detection, tray icon
toggles (healthy/unhealthy).

Analytics: opt-in `AnalyticsManager`, env-driven.

## 9. Cross-platform layer

`screenpipe-screen/src/core.rs` — `#[cfg(target_os = …)]` ветки выбирают
backend. `screenpipe-a11y/src/platform/{macos,windows_uia,linux}.rs` —
платформенные модули с единым интерфейсом.

Platform-only модули:
- `livetext_ffi.rs` — macOS Vision OCR
- `space_monitor.rs` — macOS virtual spaces
- `windows_overlay.rs`, `windows_webview_env.rs`, `windows_ca_bundle.rs`
- `owned_browser_cookies.rs` — macOS WKHTTPCookieStore (Windows/Linux stub)

## 10. Что забрать в Gilb

1. **`tokio::broadcast` singleton event bus** с TTL cleanup subscriptions —
   простой и достаточный. Не нужны actor framework'и.
2. **Разделение ServerCore (long-lived) vs CaptureSession (toggleable)** —
   permission revoke, "pause recording", restart capture без полного
   рестарта app'а — всё легко.
3. **axum** для локального HTTP API (если делаем CLI/JSON access).
4. **`local_api_key`** инжектируется во frontend — простой auth для локального
   API без OAuth-плясок.
5. **Pipes-стиль архитектура для therblig analyzers** — каждый detector =
   отдельный schedulable agent с своими permissions и triggers. Это
   натурально маппится на нашу задачу: "детектор копирования файлов",
   "детектор форматирования в Excel", и т.д.
6. **Tinfoil-style fails-closed redaction**: regex first, AI fallback,
   на ошибке — regex output, никогда не отдавать сырое.
7. **RecordingSettings shared crate** — единый source of truth для CLI и UI.
   Меньше дрейфа конфигов.
8. **`~/.screenpipe` layout** с `$SCREENPIPE_DATA_DIR` override — стандартный
   Unix-style + дружелюбно для тестов.
9. **`tracing` + Sentry + os_log** для observability с первого дня.

## 11. Что упростить для Gilb v0

- Без pipes/агентов: detection логика прямо в Rust core.
- Без Tinfoil enclave — только regex redaction (Tinfoil требует
  attestation infrastructure).
- Без HTTP API — только Tauri IPC.
- Один монолитный `GilbSettings` структура (analog `RecordingSettings`).
- Без analytics, без Sentry — потом.
