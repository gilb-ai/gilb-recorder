# Screenpipe: архитектурный обзор для Gilb

Источник: `/Users/leonid/src/gilb/reference/screenpipe` (Rust workspace + Tauri desktop app).

## Контекст для Gilb

Gilb разделён на **три логических слоя**:

| Слой | Что делает | Текущий статус |
|------|------------|----------------|
| **1. Сбор сырых данных** через a11y | поток событий + a11y дерева, dedup, batching, надёжное хранение | **в работе сейчас** |
| **2. Анализ / pattern mining** | поиск повторяющихся последовательностей (therbligs) | вне scope этой итерации |
| **3. Agent skill creation** | генерация skill'ов из найденных паттернов | вне scope этой итерации |

**Текущий этап — только Слой 1.** Цель: **идеально / масштабируемо /
устойчиво / не нагружая компьютер** собрать a11y-поток. Без качественной
нижней прослойки Слои 2-3 бессмысленны: пропуски и неточности тут ломают всё
выше.

Screenpipe — это ровно тот контракт, который мы хотим воспроизвести (и
местами улучшить) на Слое 1: long-term запись "всего что пользователь видел /
говорил / делал" с устойчивостью, дедупликацией, adaptive FPS, per-app
бюджетами, lock-free hot path, graceful degradation на потерю permission /
sleep / wake.

**Target platforms Gilb: macOS + Windows** (обе обязательны в дальнейшем).
Linux — вне scope. Это влияет на выбор абстракций: с самого начала закладываем
per-platform trait/enum в `gilb-a11y` (как у screenpipe), даже если первая
реализация будет macOS-only.

## Layout репозитория

```
screenpipe/
├── apps/
│   └── screenpipe-app-tauri/         Desktop UI (Tauri + Next.js)
├── crates/
│   ├── screenpipe-a11y/              UI events + accessibility tree capture
│   ├── screenpipe-screen/            Захват кадров (SCK/WGC/xcap)
│   ├── screenpipe-audio/             Захват и транскрипция аудио
│   ├── screenpipe-engine/            Оркестратор pipeline (VisionManager)
│   ├── screenpipe-core/              Общие примитивы, video, pipes, agents
│   ├── screenpipe-db/                SQLite + write queue + миграции
│   ├── screenpipe-events/            Pub/sub broadcast event bus
│   ├── screenpipe-config/            RecordingSettings (shared CLI/UI)
│   ├── screenpipe-redact/            PII redaction (text + image)
│   ├── screenpipe-secrets/           Keychain integration
│   ├── screenpipe-vault/             Encrypted storage
│   ├── screenpipe-connect/           Внешние интеграции (Slack, Obsidian, ...)
│   ├── screenpipe-rfdetr-mlx/        Image PII detection (ONNX)
│   └── screenpipe-apple-intelligence Apple Intelligence wrappers
├── docs/
├── VISION.md  DESIGN.md  TESTING.md  CLAUDE.md
```

## Главная архитектурная картина

```
USER ACTION
    │
    ▼
┌───────────────────────────┐    ┌───────────────────────────┐
│   screenpipe-a11y         │    │   screenpipe-screen       │
│   • CGEventTap / hooks    │    │   • ScreenCaptureKit/WGC  │
│   • AX tree walking       │    │   • OCR (Vision/Tesseract)│
│   • clipboard / app focus │    │   • snapshot JPEG writer  │
└────────────┬──────────────┘    └─────────────┬─────────────┘
             │ UiEvent + tree                  │ frame + ocr
             │ + correlation_id                │ + correlation_ids
             ▼                                 ▼
┌─────────────────────────────────────────────────────────────┐
│   screenpipe-engine  (VisionManager + Frame Linker Actor)   │
│   • broadcast::Sender<CaptureTriggerMsg> (fan-out)          │
│   • mpsc<LinkerMessage> (event ↔ frame pairing)             │
│   • per-monitor task in DashMap                             │
│   • adaptive FPS (PowerProfile watch)                       │
└────────────────────────────┬────────────────────────────────┘
                             │ async writes (batched)
                             ▼
┌─────────────────────────────────────────────────────────────┐
│   screenpipe-db  (SQLite + WAL + write queue)               │
│   • split read pool / write pool                            │
│   • WriteQueue coalesces до 500 ops в одну TX               │
│   • FTS5 для текста, sqlite-vec для embeddings              │
│   • media on disk, soft-evict в БД                          │
└────────────────────────────┬────────────────────────────────┘
                             │ tokio::sync::broadcast
                             ▼
┌─────────────────────────────────────────────────────────────┐
│   screenpipe-events  (workflow/permission/meeting/pipe)     │
│   → screenpipe-core/pipes (AI agents, cron, triggers)       │
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
| `00-overview.md` | (этот) карта репо + общая картина + 3-слойная рамка |
| `01-a11y-capture.md` | как устроен `screenpipe-a11y` (детально) |
| `02-storage.md` | схема БД, write queue, retention, FTS, embeddings |
| `03-screen-pipeline.md` | захват экрана, OCR, frame linker, multi-monitor (для Слоёв 2+) |
| `04-events-and-integration.md` | event bus, Tauri IPC, HTTP API, pipes, redact, config |
| `05-gilb-recommendations.md` | дорожная карта Слоя 1, схема БД, что пропустить |
| **`06-layer1-capture-quality.md`** | **главный документ итерации**: completeness / scalability / robustness / lightweight + чек-лист готовности Слоя 1 |
