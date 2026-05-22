# Рекомендации для Gilb (на основе разбора prior-art)

Этот файл — синтез: что **взять напрямую**, что **адаптировать**, что
**пропустить** в первой версии Gilb.

## Архитектурная рамка: 3 слоя

| Слой | Что | Текущий статус |
|------|-----|----------------|
| **1. Сбор сырых данных** через a11y | event stream + tree snapshots + надёжное хранение | **ТЕКУЩИЙ ФОКУС** |
| 2. Анализ / pattern mining (therbligs) | поиск повторяющихся последовательностей | отложено |
| 3. Agent skill creation | генерация автоматизаций из паттернов | отложено |

**Этот документ описывает только Слой 1.** Детальный чек-лист качества
Слоя 1 (completeness / scalability / robustness / lightweight) — в
[`06-layer1-capture-quality.md`](06-layer1-capture-quality.md).

Слои 2-3 упоминаются ниже исключительно как контракт ("какие гарантии Слоя 1
им нужны"), **не реализуем сейчас**.

**Target platforms: macOS + Windows (обе обязательны).** Linux вне scope. MVP
делаем macOS-first, но архитектура с самого начала должна быть расширяема на
Windows — без macOS-specific API в публичных интерфейсах `gilb-a11y` /
`gilb-engine`.

## A. Что взять напрямую (proven patterns)

### 1. Структура crates (для Слоя 1)

```
gilb/
├── apps/gilb-app-tauri/          UI (минимальный — статус записи, пауза, перм-чек)
├── crates/
│   ├── gilb-a11y/                ★★★ event capture (analog prior-art)
│   ├── gilb-db/                  ★★★ SQLite + write queue + retention
│   ├── gilb-engine/              ★★  orchestration (capture session lifecycle)
│   ├── gilb-core/                ★   shared types (Action, ElementContext, ...)
│   ├── gilb-config/              ★   shared settings
│   └── gilb-events/              ★   broadcast pub/sub (permission/health events)
```

`gilb-detector` (Слой 2) и agent skill runtime (Слой 3) — отложены, добавим
позже. Crates обозначены ★★★ — критичные для Слоя 1 сейчас; ★ — нужны, но
минималистично.

### 2. A11y capture (1-в-1 со prior-art, см. `01-a11y-capture.md`)

Платформо-независимые механики (нужны и на macOS, и на Windows):

- **TextBuffer** аггрегатор с timeout 300 мс.
- **Adaptive FPS** (`activity_feed.rs`).
- **Per-app `WalkBudget`** (Light/Moderate/Heavy/Critical).
- **`ElementContext`** на click + **`AccessibilityNode`** tree snapshots.
- **`SimHash` dedup** в `TreeCache`.
- **`ArcSwap`** для lock-free current_app/window.

Platform-specific backends:

**macOS** (`platform/macos.rs` стиль):
- **CGEventTap** (`LISTEN_ONLY`, через `prior-art`) + worker threads для AX
  context.
- **`AX_QUERY_LOCK` mutex** для серилизации AX queries; try-lock в hot path.
- **AX Observer** на focused pid, переподписка на app switch.
- **Clipboard poller** 750 мс (`NSPasteboard.changeCount()`).

**Windows** (`platform/windows_uia.rs` стиль):
- **SetWindowsHookEx** (WH_MOUSE_LL + WH_KEYBOARD_LL) на отдельном потоке.
- **UIA `IUIAutomation`** через crate `windows@0.58` на apartment-threaded
  worker'е, с **CacheRequest batching** (одна COM call = все свойства
  subtree).
- **Control View + TreeWalker fallback** для Chromium/Electron.
- **`IUIAutomationFocusChangedEventHandler`** для focus tracking.

Это всё кодом обкатано — можно реально форкнуть `prior-art` целиком как
стартовую точку и срезать только Linux-ветку, оставив macOS и Windows.

### 3. SQLite tuning (1-в-1 со prior-art-db, см. `02-storage.md`)

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA cache_size = -65536;       -- 64 MB
PRAGMA mmap_size = 268435456;     -- 256 MB
PRAGMA temp_store = MEMORY;
PRAGMA wal_autocheckpoint = 4000;
PRAGMA busy_timeout = 5000;
```

+ split **read pool** / **write pool**, **single-permit write semaphore**,
**ImmediateTx** wrapper, **WriteQueue batching** (≤500 ops per TX).

### 4. Frame ↔ event linking через correlation_id

**Отложено на Слой 1.** Snapshot экрана в v0 не делаем — pure a11y stream
(см. §C). Когда добавим snapshot'ы — берём **`FrameLinker` actor pattern**
(см. `03-screen-pipeline.md` §7), он решает order-independent матчинг без
timestamp-based hacks.

### 5. ServerCore vs CaptureSession разделение

Tauri app должен уметь pause/resume recording без рестарта. Разделение из
prior-art (см. `04-events-and-integration.md` §2) — идеальная модель.

### 6. `~/.gilb` layout с `$GILB_DATA_DIR` override

```
~/.gilb/
├── db.sqlite
├── snapshots/YYYY-MM-DD/{ts}_m{monitor}.jpg
├── config.toml
└── logs/
```

## B. Что адаптировать под Слой 1 Gilb

### 1. Схема БД — минимальная, под чистый event log

Слой 1 пишет **действия и контекст**. Анализ — отдельная подсистема Слоя 2,
поэтому здесь **нет** таблиц `therbligs` / `therblig_instances` / `detector
runs`. Их добавим позже миграциями.

```sql
-- сессии записи (между паузами, перезапусками, sleep'ами)
CREATE TABLE sessions (
    id INTEGER PRIMARY KEY,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    device_id TEXT NOT NULL,
    gilb_version TEXT NOT NULL,
    stop_reason TEXT                       -- paused / sleep / crash / permission_lost / shutdown
);

-- атомарное действие — единица потока Слоя 1
CREATE TABLE actions (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,                    -- ms epoch
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    kind TEXT NOT NULL,                     -- click|move|scroll|key|text|app_switch|window_focus|element_focus|clipboard
    app_name TEXT,
    app_pid INTEGER,
    window_title TEXT,
    -- payload по kind:
    x INTEGER, y INTEGER, button INTEGER, click_count INTEGER,
    delta_x INTEGER, delta_y INTEGER,
    key_code INTEGER, modifiers INTEGER,
    text_content TEXT,
    text_char_count INTEGER,
    clipboard_op TEXT,                      -- c|x|v|p
    -- a11y контекст кликнутого / сфокусированного элемента:
    element_role TEXT,
    element_name TEXT,
    element_value TEXT,                     -- с password-field маскированием
    element_automation_id TEXT,
    element_bounds_json TEXT,
    -- редакция:
    is_password_context INTEGER DEFAULT 0,
    -- быстрый dedup и потенциальное связывание с tree_snapshot:
    tree_snapshot_id INTEGER REFERENCES tree_snapshots(id)
);
CREATE INDEX idx_actions_ts             ON actions(ts);
CREATE INDEX idx_actions_session_ts     ON actions(session_id, ts);
CREATE INDEX idx_actions_app_ts         ON actions(app_name, ts);
CREATE INDEX idx_actions_kind_ts        ON actions(kind, ts);

-- periodic a11y tree snapshots (по focus change + SimHash dedup)
CREATE TABLE tree_snapshots (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    app_name TEXT, window_title TEXT, pid INTEGER,
    tree_hash INTEGER NOT NULL,             -- SimHash для дедупа
    element_count INTEGER,
    root_json TEXT,                         -- gzipped/zstd по желанию
    walk_duration_ms INTEGER,
    truncated INTEGER DEFAULT 0
);
CREATE INDEX idx_trees_ts        ON tree_snapshots(ts);
CREATE INDEX idx_trees_app_ts    ON tree_snapshots(app_name, ts);
CREATE INDEX idx_trees_hash      ON tree_snapshots(tree_hash);

-- per-app capture бюджет (для наблюдения и persistent throttling)
CREATE TABLE app_budgets (
    app_name TEXT PRIMARY KEY,
    tier TEXT NOT NULL,                     -- light|moderate|heavy|critical
    last_walk_ms INTEGER,
    avg_walk_ms REAL,
    truncated_count INTEGER DEFAULT 0,
    updated_at INTEGER NOT NULL
);

-- health/observability события
CREATE TABLE health_events (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    kind TEXT NOT NULL,                     -- permission_lost|permission_restored|wake|crash|drop|...
    detail TEXT
);

-- FTS5 поверх actions для дебага и Слоя 2 потом:
CREATE VIRTUAL TABLE actions_fts USING fts5(
    text_content, element_name, window_title, app_name,
    content='actions', content_rowid='id'
);
-- + AFTER INSERT/UPDATE/DELETE triggers (синхронные, см. prior-art-db §8)
```

### 2. Soft-evict policy

prior-art evict'ит media, сохраняет rows. Для Gilb на Слое 1:
- `actions` — **никогда не удаляем** (малый размер, нужен Слою 2 на годы).
- `tree_snapshots.root_json` — evict через N дней (хранится hash + metadata).
- `health_events` — rotate по count (последние 10 000).

### 3. Snapshot-on-trigger (когда добавим экранные кадры)

**Не в v0 Слоя 1.** Когда дойдём — снимаем кадр на:
- focus change / app switch,
- click по элементу с cache-miss контекстом,
- значимое визуальное изменение.

См. `03-screen-pipeline.md` §2 (event-driven, debounce 500 мс) +
correlation_id для связывания (см. §A.4).

### 4. Контракт API между Слоем 1 и будущим Слоем 2

Чтобы Слой 2 потом не требовал переписать Слой 1, фиксируем контракт сейчас:

- Все actions **ordered по `(session_id, ts, id)`** — гарантия из write
  queue (single-writer per session).
- `kind` — закрытое перечисление, расширяется только миграциями.
- `element_*` поля либо все NULL (нет AX context), либо все вместе.
- `is_password_context = 1` ⇒ `text_content` и `element_value` зачищены.
- Между `actions` и `tree_snapshots` связь идёт через `tree_snapshot_id`,
  не через timestamp.

Это даёт Слою 2 чистый stream без сюрпризов.

## C. Что пропустить в v0 Слоя 1

Цель — не тащить ничего, что не нужно для **качественного сбора** прямо
сейчас. Всё остальное добавим, когда Слой 1 пройдёт чек-лист из
`06-layer1-capture-quality.md` §5.

| Из prior-art | Почему пропускаем |
|---------------|-------------------|
| **Слой 2: therblig detection** | отдельная подсистема, делаем после стабильного Слоя 1 |
| **Слой 3: agent skill / replay** | ещё позже |
| Linux в a11y | вне scope Gilb навсегда |
| Windows в **v0 MVP** (но в v1 обязательно) | macOS-first старт; Windows-ветка идёт сразу после MVP. Архитектуру под обе платформы закладываем сразу (per-platform trait) |
| Screen video / snapshots | в v0 Слой 1 — **pure a11y stream**. Snapshot'ы добавим как опцию позже, когда основной stream стабилен |
| ScreenCaptureKit + ffmpeg видео | snapshot'ов достаточно (и тех — позже) |
| OCR pipeline (Vision/Tesseract) | a11y текста достаточно; OCR — feature Слоя 2 при необходимости |
| Audio capture / diarization | вне scope, audio не относится к a11y потоку |
| sqlite-vec embeddings | для Слоя 2 |
| Tinfoil PII enclave | regex redaction + password-field detection хватит на Слое 1 |
| Pipes / Agent executors | это инфра Слоя 3 |
| Cloud sync, multi-device | local-only v0 |
| HTTP API (axum) | только Tauri IPC для UI статуса; API наружу — позже |
| Meeting detection | вне scope |
| Image PII (RFDETR ONNX) | вне scope |
| sentry / analytics | потом |

## D. Карта зависимостей (Cargo) для MVP

```toml
[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
sqlx = { version = "0.7", features = ["sqlite", "runtime-tokio-rustls", "migrate"] }
arc-swap = "1"
parking_lot = "0.12"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tracing-appender = "0.2"
anyhow = "1"
thiserror = "1"
dashmap = "5"
arboard = "3"                       # clipboard, both platforms

[target.'cfg(target_os = "macos")'.dependencies]
prior-art = { version = "0.13", features = ["ax", "cg", "blocks", "ns", "dispatch"] }
tracing-oslog = "0.2"

[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "0.58", features = [
    "Win32_Foundation",
    "Win32_UI_Accessibility",
    "Win32_UI_WindowsAndMessaging",
    "Win32_System_Com",
    "Win32_System_Ole",
] }
```

## E. Дорожная карта Gilb (Слой 1 → 2 → 3)

### Слой 1 — текущий фокус

**v0.1 — walking skeleton (macOS):**
1. Скелет crates с per-platform `mod platform` (`macos.rs` реализован,
   `windows.rs` — trait-stub).
2. Тривиальный `gilb-db` со схемой §B (sessions + actions + tree_snapshots
   + app_budgets + health_events). Без write_queue, без FTS.
3. CGEventTap + AX observer для focused pid, write actions в SQLite.
4. Просмотр через `sqlite3 ~/.gilb/db.sqlite "select … from actions limit 50"`.

**v0.2 — completeness:**
5. Clipboard poller (NSPasteboard.changeCount() 750 мс).
6. TextBuffer аггрегатор 300 мс.
7. AppSwitch / WindowFocus / FocusedUIElement.
8. ElementContext capture на click (через worker thread + AX_QUERY_LOCK
   try-lock).

**v0.3 — lightweight / scalable:**
9. Adaptive FPS (activity_feed.rs стиль).
10. Per-app WalkBudget с persisted state в `app_budgets`.
11. SimHash dedup tree snapshots.
12. Write queue с батчингом ≤500 ops/TX, ImmediateTx, WAL tuning.

**v0.4 — robustness:**
13. Bounded channels everywhere + drop-with-warn.
14. ArcSwap для current_app/window.
15. Sleep monitor (CFNotification) → stream re-create.
16. Permission lost/restored detection + health_events publish.
17. `gilb db recover` CLI (VACUUM/REINDEX/integrity_check).
18. Excluded apps list (1Password, Bitwarden, KeePassXC, ...) + password
    field detection + regex PII redactor.

**v0.5 — UI и pause:**
19. Tauri app: tray icon (recording / paused / unhealthy), глобальная пауза,
    permission status, simple "сегодня записано N actions" view.
20. ServerCore vs CaptureSession разделение (см. §A.5).

**v0.6 — Windows backend:**
21. `SetWindowsHookEx` (WH_MOUSE_LL + WH_KEYBOARD_LL) + UIA через crate
    `windows`, CacheRequest batching, focus event handler.
22. CI: windows-latest runner.
23. Прогон того же стресс-теста "8 ч реальной работы" на Windows.

→ **Gate**: чек-лист `06-layer1-capture-quality.md` §5 закрыт на обеих
платформах. Только после этого начинаем Слой 2.

### Слой 2 — потом (вне scope сейчас)

- Detector trait, первые правила (clipboard copy/paste pair, drag-drop,
  N-gram repetition).
- Sliding-window pattern mining.
- Snapshot capture on triggers (Frame Linker pattern, ScreenCaptureKit на
  macOS, Windows Graphics Capture на Windows).
- UI для просмотра найденных therblig'ов.

### Слой 3 — ещё позже

- Agent skill generation из найденных паттернов.
- Replay / автоматизация.
- Pipes-style система с permissions.

## F. Ссылки на ключевые файлы prior-art

| Что | Файл |
|-----|------|
| macOS event tap + AX | `crates/prior-art/src/platform/macos.rs:614-700` |
| AX query lock | `crates/prior-art/src/platform/macos.rs:28` |
| Activity feed FPS | `crates/prior-art/src/activity_feed.rs:92-134` |
| App walk budget | `crates/prior-art/src/budget.rs:14-194` |
| Tree SimHash cache | `crates/prior-art/src/tree/cache.rs` |
| EventData enum | `crates/prior-art/src/events.rs:47-124` |
| DB pools + semaphores | `crates/prior-art-db/src/db.rs:186-209` |
| ImmediateTx | `crates/prior-art-db/src/db.rs:104-184` |
| Write queue | `crates/prior-art-db/src/write_queue.rs` |
| Frame Linker actor | `crates/prior-art-engine/src/frame_linker.rs`, `frame_linker_actor.rs` |
| Event-driven capture | `crates/prior-art-engine/src/event_driven_capture.rs` |
| Paired capture (hybrid OCR) | `crates/prior-art-engine/src/paired_capture.rs:48-200` |
| Event bus | `crates/prior-art-events/src/events_manager.rs:12-33` |
| ServerCore | `apps/prior-art-app-tauri/src-tauri/src/server_core.rs:1-120` |
| RecordingSettings | `crates/prior-art-config/src/recording.rs` |
| Redact pipeline | `crates/prior-art-redact/src/pipeline.rs` |
| Regex patterns | `crates/prior-art-redact/src/adapters/regex.rs:44-100` |
| VISION/принципы | `VISION.md`, `DESIGN.md`, `CLAUDE.md` |
