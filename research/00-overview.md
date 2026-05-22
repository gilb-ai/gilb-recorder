# prior-art: архитектурный обзор для Gilb

Источник: `/Users/leonid/src/gilb/reference/prior-art` (Rust workspace + Tauri desktop app).

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

prior-art — это ровно тот контракт, который мы хотим воспроизвести (и
местами улучшить) на Слое 1: long-term запись "всего что пользователь видел /
говорил / делал" с устойчивостью, дедупликацией, adaptive FPS, per-app
бюджетами, lock-free hot path, graceful degradation на потерю permission /
sleep / wake.

**Target platforms Gilb: macOS + Windows** (обе обязательны в дальнейшем).
Linux — вне scope. Это влияет на выбор абстракций: с самого начала закладываем
per-platform trait/enum в `gilb-a11y` (как у prior-art), даже если первая
реализация будет macOS-only.

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
| `00-overview.md` | (этот) карта репо + общая картина + 3-слойная рамка |
| `01-a11y-capture.md` | как устроен `prior-art` (детально) |
| `02-storage.md` | схема БД, write queue, retention, FTS, embeddings |
| `03-screen-pipeline.md` | захват экрана, OCR, frame linker, multi-monitor (для Слоёв 2+) |
| `04-events-and-integration.md` | event bus, Tauri IPC, HTTP API, pipes, redact, config |
| `05-gilb-recommendations.md` | дорожная карта Слоя 1, схема БД, что пропустить |
| **`06-layer1-capture-quality.md`** | **главный документ итерации**: completeness / scalability / robustness / lightweight + чек-лист готовности Слоя 1 |
