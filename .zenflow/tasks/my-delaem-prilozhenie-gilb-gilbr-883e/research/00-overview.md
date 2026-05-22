# prior-art: архитектурный обзор для Gilb

Источник: `/Users/leonid/src/gilb/prior-art` (Rust workspace + Tauri desktop app).

## Контекст для Gilb

Gilb записывает действия пользователя через accessibility API и ищет повторяющиеся
паттерны (therbligs). prior-art решает очень близкую задачу — long-term запись
"всего что пользователь видел/говорил/делал" с поиском по содержимому. Поэтому его
архитектура — это готовый шаблон того, **что работает на проде** в этом домене.

## Layout репозитория

```
prior-art/
├── apps/
│   └── prior-art-app-tauri/         Desktop UI (Tauri + Next.js)
├── crates/
│   ├── prior-art/              UI events + accessibility tree capture
│   ├── prior-art-screen/            Захват кадров (SCK/WGC/xcap)
│   ├── prior-art-audio/             Захват и транскрипция аудио
│   ├── prior-art-engine/            Оркестратор pipeline (VisionManager)
│   ├── prior-art-core/              Общие примитивы, video, pipes, agents
│   ├── prior-art-db/                SQLite + write queue + миграции
│   ├── prior-art-events/            Pub/sub broadcast event bus
│   ├── prior-art-config/            RecordingSettings (shared CLI/UI)
│   ├── prior-art-redact/            PII redaction (text + image)
│   ├── prior-art-secrets/           Keychain integration
│   ├── prior-art-vault/             Encrypted storage
│   ├── prior-art-connect/           Внешние интеграции (Slack, Obsidian, ...)
│   ├── prior-art-rfdetr-mlx/        Image PII detection (ONNX)
│   └── prior-art-apple-intelligence Apple Intelligence wrappers
├── docs/
├── VISION.md  DESIGN.md  TESTING.md  CLAUDE.md
```

## Главная архитектурная картина

```
USER ACTION
    │
    ▼
┌───────────────────────────┐    ┌───────────────────────────┐
│   prior-art         │    │   prior-art-screen       │
│   • CGEventTap / hooks    │    │   • ScreenCaptureKit/WGC  │
│   • AX tree walking       │    │   • OCR (Vision/Tesseract)│
│   • clipboard / app focus │    │   • snapshot JPEG writer  │
└────────────┬──────────────┘    └─────────────┬─────────────┘
             │ UiEvent + tree                  │ frame + ocr
             │ + correlation_id                │ + correlation_ids
             ▼                                 ▼
┌─────────────────────────────────────────────────────────────┐
│   prior-art-engine  (VisionManager + Frame Linker Actor)   │
│   • broadcast::Sender<CaptureTriggerMsg> (fan-out)          │
│   • mpsc<LinkerMessage> (event ↔ frame pairing)             │
│   • per-monitor task in DashMap                             │
│   • adaptive FPS (PowerProfile watch)                       │
└────────────────────────────┬────────────────────────────────┘
                             │ async writes (batched)
                             ▼
┌─────────────────────────────────────────────────────────────┐
│   prior-art-db  (SQLite + WAL + write queue)               │
│   • split read pool / write pool                            │
│   • WriteQueue coalesces до 500 ops в одну TX               │
│   • FTS5 для текста, sqlite-vec для embeddings              │
│   • media on disk, soft-evict в БД                          │
└────────────────────────────┬────────────────────────────────┘
                             │ tokio::sync::broadcast
                             ▼
┌─────────────────────────────────────────────────────────────┐
│   prior-art-events  (workflow/permission/meeting/pipe)     │
│   → prior-art-core/pipes (AI agents, cron, triggers)       │
│   → Tauri app + HTTP API (axum, port 3030)                  │
└─────────────────────────────────────────────────────────────┘
```

## Ключевые принципы (из VISION.md)

- **Local-first**: все данные локально по умолчанию, sync — encrypted opt-in.
- **Stability over features**: только три глагола — Record / Rewind / Ask.
- **Respect the machine**: бюджет < 20% CPU, < 3 GB RAM.
- **Cross-platform**: macOS / Windows / Linux с честными нативными API
  (никаких `rdev`-style надстроек).
- **Screen = универсальный интерфейс**: 10 Mbit/s сигнала human intent.

## Документы в этой папке

| Файл | О чём |
|------|-------|
| `00-overview.md` | (этот) карта репо + общая картина |
| `01-a11y-capture.md` | как устроен `prior-art` (детально) |
| `02-storage.md` | схема БД, write queue, retention, FTS, embeddings |
| `03-screen-pipeline.md` | захват экрана, OCR, frame linker, multi-monitor |
| `04-events-and-integration.md` | event bus, Tauri IPC, HTTP API, pipes, redact, config |
| `05-gilb-recommendations.md` | что взять / адаптировать / упростить для Gilb |
