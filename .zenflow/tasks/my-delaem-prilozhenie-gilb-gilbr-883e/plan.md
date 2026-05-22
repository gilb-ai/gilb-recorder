# План: разбор архитектуры screenpipe для Gilb

## Цель

Понять, как screenpipe реализует запись через a11y, хранение длительных
данных, оркестрацию pipeline и интеграцию с UI. Зафиксировать архитектурные
находки в md-файлах, на которых можно потом построить v0 Gilb.

## Результат

Все находки записаны в `research/`:

- [`research/00-overview.md`](research/00-overview.md) — карта репо + общая
  архитектурная картина + ключевые принципы из VISION.md.
- [`research/01-a11y-capture.md`](research/01-a11y-capture.md) — детально про
  `screenpipe-a11y`: CGEventTap, AX observer, clipboard poller, adaptive FPS,
  per-app walk budget, SimHash dedup, lock-free hot path, privacy.
- [`research/02-storage.md`](research/02-storage.md) — `screenpipe-db`:
  схема таблиц, split read/write pools, write queue с батчингом 500 ops/TX,
  ImmediateTx, FTS5, embeddings (sqlite-vec), soft-evict retention, PRAGMA
  tuning по tier'ам.
- [`research/03-screen-pipeline.md`](research/03-screen-pipeline.md) —
  захват экрана (SCK/WGC/xcap), hybrid OCR (a11y first), VisionManager,
  Frame Linker actor с correlation_id, multi-monitor, power awareness.
- [`research/04-events-and-integration.md`](research/04-events-and-integration.md) —
  event bus (`tokio::broadcast` singleton), Tauri ↔ Rust (ServerCore vs
  CaptureSession), HTTP API (axum), pipes/агенты, redact pipeline,
  RecordingSettings, storage layout, observability.
- [`research/05-gilb-recommendations.md`](research/05-gilb-recommendations.md) —
  синтез: что взять напрямую, что адаптировать под therblig'и, что пропустить
  в v0; набросок схемы БД под therblig mining; дорожная карта v0 → v1.

## Шаги исследования

### [x] Step: Разобрать структуру репозитория screenpipe

Прочитал `CLAUDE.md`, `DESIGN.md`, корневые директории `apps/` и `crates/`.
Зафиксировано в `research/00-overview.md`.

### [x] Step: Глубокий разбор screenpipe-a11y

Развёрнут Explore-agent на crate `screenpipe-a11y`. Покрыто: API/деps,
mac/win/linux backends, типы событий, hybrid event mechanism, adaptive FPS,
per-app budget, dedup, privacy, lock-free patterns. Результат в
`research/01-a11y-capture.md`.

### [x] Step: Глубокий разбор screenpipe-db

Развёрнут Explore-agent на crate `screenpipe-db`. Покрыто: SQLite + WAL,
86 миграций, схема всех таблиц (frames, ui_events, elements, embeddings,
diarization, memories, meetings), write queue, pool architecture, FTS5,
retention policy, recovery. Результат в `research/02-storage.md`.

### [x] Step: Pipeline захвата экрана и оркестрации

Развёрнут Explore-agent на screen/engine/core. Покрыто: SCK/WGC/xcap,
event-driven capture, hybrid OCR с semaphore, Frame Linker actor pattern,
multi-monitor, power awareness, DRM detection, lifecycle. Результат в
`research/03-screen-pipeline.md`.

### [x] Step: События, интеграция с Tauri, redact, config

Развёрнут Explore-agent на events/redact/config/Tauri app. Покрыто:
broadcast event bus с TTL cleanup, custom events, ServerCore vs CaptureSession,
HTTP API (axum), pipes/AgentExecutor система, Tinfoil PII + ONNX image
redaction, RecordingSettings, storage layout, observability stack. Результат
в `research/04-events-and-integration.md`.

### [x] Step: Синтез рекомендаций для Gilb

Сведено в `research/05-gilb-recommendations.md`: что взять напрямую (a11y,
SQLite tuning, write queue, Frame Linker, ServerCore split), что адаптировать
(схема БД под therblig mining, snapshot-on-trigger вместо непрерывного видео,
detector trait), что пропустить в v0 (Win/Linux, OCR, audio, embeddings,
Tinfoil, pipes, cloud sync), плюс дорожная карта v0 → v1.

## Что НЕ покрыто в этом исследовании (для следующих итераций)

- Глубокий разбор `screenpipe-audio` (вне scope therblig'ов).
- `screenpipe-vault` (encrypted storage) — посмотреть когда будем делать
  privacy-sensitive хранение.
- `screenpipe-connect` (Slack/Obsidian) — релевантно когда дойдём до экспорта.
- `screenpipe-apple-intelligence` — посмотреть когда будем интегрировать
  LLM-классификацию паттернов.
- Frontend паттерны (`apps/screenpipe-app-tauri/`) — разобрать перед началом
  работы над UI Gilb.
