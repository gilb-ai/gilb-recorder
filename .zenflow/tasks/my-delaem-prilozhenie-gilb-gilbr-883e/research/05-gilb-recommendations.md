# Рекомендации для Gilb (на основе разбора screenpipe)

Этот файл — синтез: что **взять напрямую**, что **адаптировать**, что
**пропустить** в первой версии Gilb.

Напоминание: задача Gilb — записывать a11y-поток действий пользователя и
находить **повторяющиеся последовательности (therbligs)**.

**Target platforms: macOS + Windows (обе обязательны).** Linux вне scope. MVP
делаем macOS-first, но архитектура с самого начала должна быть расширяема на
Windows — без macOS-specific API в публичных интерфейсах `gilb-a11y` /
`gilb-engine`.

## A. Что взять напрямую (proven patterns)

### 1. Структура crates

```
gilb/
├── apps/gilb-app-tauri/          UI
├── crates/
│   ├── gilb-a11y/                ★ event capture (analog screenpipe-a11y)
│   ├── gilb-db/                  ★ SQLite + write queue
│   ├── gilb-engine/              orchestration
│   ├── gilb-core/                shared types
│   ├── gilb-detector/            ★ therblig detection (НОВОЕ для нас)
│   ├── gilb-config/              shared settings
│   └── gilb-events/              broadcast pub/sub
```

### 2. A11y capture (1-в-1 со screenpipe-a11y, см. `01-a11y-capture.md`)

Платформо-независимые механики (нужны и на macOS, и на Windows):

- **TextBuffer** аггрегатор с timeout 300 мс.
- **Adaptive FPS** (`activity_feed.rs`).
- **Per-app `WalkBudget`** (Light/Moderate/Heavy/Critical).
- **`ElementContext`** на click + **`AccessibilityNode`** tree snapshots.
- **`SimHash` dedup** в `TreeCache`.
- **`ArcSwap`** для lock-free current_app/window.

Platform-specific backends:

**macOS** (`platform/macos.rs` стиль):
- **CGEventTap** (`LISTEN_ONLY`, через `cidre`) + worker threads для AX
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

Это всё кодом обкатано — можно реально форкнуть `screenpipe-a11y` целиком как
стартовую точку и срезать только Linux-ветку, оставив macOS и Windows.

### 3. SQLite tuning (1-в-1 со screenpipe-db, см. `02-storage.md`)

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

Если будем снимать snapshot экрана при действиях — **`FrameLinker` actor
pattern** (см. `03-screen-pipeline.md` §7) уже решает order-independent
матчинг. Никаких timestamp-based hacks.

### 5. ServerCore vs CaptureSession разделение

Tauri app должен уметь pause/resume recording без рестарта. Разделение из
screenpipe (см. `04-events-and-integration.md` §2) — идеальная модель.

### 6. `~/.gilb` layout с `$GILB_DATA_DIR` override

```
~/.gilb/
├── db.sqlite
├── snapshots/YYYY-MM-DD/{ts}_m{monitor}.jpg
├── config.toml
└── logs/
```

## B. Что адаптировать под therblig'и

### 1. Схема БД — упростить и переориентировать

Screenpipe оптимизирован под **search "когда я последний раз видел X"**.
Gilb — под **pattern mining "какие последовательности действий повторяются"**.

Предлагаемая схема (MVP):

```sql
-- атомарное действие (therblig)
CREATE TABLE actions (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,                    -- ms since epoch
    session_id INTEGER NOT NULL,            -- сессия пользователя
    kind TEXT NOT NULL,                     -- click/key/text/scroll/app_switch/...
    app_name TEXT,
    app_pid INTEGER,
    window_title TEXT,
    -- payload зависит от kind:
    x INTEGER, y INTEGER, button INTEGER,
    key_code INTEGER, modifiers INTEGER,
    text_content TEXT,
    -- контекст a11y:
    element_role TEXT,
    element_name TEXT,
    element_value TEXT,
    element_automation_id TEXT,
    element_bounds_json TEXT,
    -- дедупликация:
    context_hash INTEGER,                   -- быстрый exact match
    snapshot_path TEXT
);
CREATE INDEX idx_actions_ts ON actions(ts);
CREATE INDEX idx_actions_session ON actions(session_id, ts);
CREATE INDEX idx_actions_app ON actions(app_name, ts);
CREATE INDEX idx_actions_context_hash ON actions(context_hash);

-- a11y дерево (опционально, periodic snapshots)
CREATE TABLE ui_trees (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    app_name TEXT, window_title TEXT, pid INTEGER,
    tree_hash INTEGER,                      -- SimHash для dedup
    element_count INTEGER,
    root_json TEXT                          -- compressed JSON
);

-- найденные паттерны
CREATE TABLE therbligs (
    id INTEGER PRIMARY KEY,
    label TEXT,                             -- "copy file to dropbox"
    fingerprint_hash INTEGER,
    occurrence_count INTEGER,
    avg_duration_ms INTEGER,
    last_seen_at INTEGER
);

CREATE TABLE therblig_instances (
    id INTEGER PRIMARY KEY,
    therblig_id INTEGER NOT NULL,
    start_ts INTEGER NOT NULL,
    end_ts INTEGER NOT NULL,
    confidence REAL,
    action_ids_json TEXT
);
```

FTS5 потом (`actions_fts(text_content, element_name, window_title)`) — для
"искать действия с текстом X".

### 2. Soft-evict policy

Screenpipe evict'ит media но сохраняет rows. Для Gilb evict'ить **snapshots**,
но **никогда не actions** — они нужны для history mining.

### 3. Detector — новая абстракция

```rust
trait Detector {
    fn name(&self) -> &str;
    fn process(&mut self, actions: &[Action]) -> Vec<TherbligCandidate>;
}
```

Запускается на batch'ах из write_queue (post-commit hook?) или periodic
sliding-window job. Pipes-style (см. `04`) — overkill для v0; начинаем с
hard-coded detectors.

### 4. Snapshot — только при значимых триггерах

Не на каждый click, а на:
- focus change
- app switch
- click по элементу с уже неизвестной структурой (cache miss)
- значимое визуальное изменение

Это уже описано в `03-screen-pipeline.md` §2 (event-driven, debounce 500 мс).

## C. Что пропустить в v0

| Из screenpipe | Почему пропускаем |
|---------------|-------------------|
| Linux в a11y | вне scope Gilb |
| Windows в **v0 MVP** (но в v1 обязательно) | macOS-first старт; Windows-ветка добавляется сразу после MVP — архитектуру под обе платформы закладываем сразу |
| ScreenCaptureKit видео + ffmpeg | snapshot'ов достаточно |
| OCR pipeline (Vision/Tesseract) | a11y текста достаточно для большинства apps |
| Audio capture/diarization | вне scope therblig'ов |
| sqlite-vec embeddings | пока не нужно |
| Tinfoil PII enclave | regex redaction хватит |
| Pipes / Agent executors | detector логика в Rust |
| Cloud sync, multi-device | local-only v0 |
| HTTP API (axum) | только Tauri IPC |
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
cidre = { version = "0.13", features = ["ax", "cg", "blocks", "ns", "dispatch"] }
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

## E. Дорожная карта Gilb v0 → v1

**v0 (proof of concept, macOS-only)** — 1-2 недели:
1. Скопировать структуру `screenpipe-a11y` с per-platform `mod platform` —
   реализован только `macos.rs`, `windows.rs` существует как заглушка trait'а.
2. Тривиальный `gilb-db` со схемой выше, без write_queue.
3. CLI binary, который пишет actions в SQLite.
4. Просмотр через `sqlite3 ~/.gilb/db.sqlite "select * from actions limit 50"`.

**v0.5 (macOS совершенствуем)**:
5. Adaptive FPS + per-app WalkBudget.
6. `SimHash` дедупликация tree snapshots.
7. Write queue с батчингом.
8. Tauri app: simple timeline view.

**v1 (Windows + therblig detection)**:
9. **Windows backend**: `SetWindowsHookEx` + UIA через crate `windows`,
   CacheRequest batching, focus event handler. CI добавляет windows runner.
10. Detector trait + первые правила (clipboard copy/paste pair, drag-drop,
    N-gram repetition).
11. Sliding-window pattern mining.
12. Snapshot capture на triggers (Frame Linker pattern, ScreenCaptureKit на
    macOS, Windows Graphics Capture на Windows).
13. UI для просмотра найденных therblig'ов и их инстансов.

**v1.5+**: PII redaction, multi-monitor, экспорт паттернов, AI-эвристики
("это похоже на форму отправки отчёта").

## F. Ссылки на ключевые файлы screenpipe

| Что | Файл |
|-----|------|
| macOS event tap + AX | `crates/screenpipe-a11y/src/platform/macos.rs:614-700` |
| AX query lock | `crates/screenpipe-a11y/src/platform/macos.rs:28` |
| Activity feed FPS | `crates/screenpipe-a11y/src/activity_feed.rs:92-134` |
| App walk budget | `crates/screenpipe-a11y/src/budget.rs:14-194` |
| Tree SimHash cache | `crates/screenpipe-a11y/src/tree/cache.rs` |
| EventData enum | `crates/screenpipe-a11y/src/events.rs:47-124` |
| DB pools + semaphores | `crates/screenpipe-db/src/db.rs:186-209` |
| ImmediateTx | `crates/screenpipe-db/src/db.rs:104-184` |
| Write queue | `crates/screenpipe-db/src/write_queue.rs` |
| Frame Linker actor | `crates/screenpipe-engine/src/frame_linker.rs`, `frame_linker_actor.rs` |
| Event-driven capture | `crates/screenpipe-engine/src/event_driven_capture.rs` |
| Paired capture (hybrid OCR) | `crates/screenpipe-engine/src/paired_capture.rs:48-200` |
| Event bus | `crates/screenpipe-events/src/events_manager.rs:12-33` |
| ServerCore | `apps/screenpipe-app-tauri/src-tauri/src/server_core.rs:1-120` |
| RecordingSettings | `crates/screenpipe-config/src/recording.rs` |
| Redact pipeline | `crates/screenpipe-redact/src/pipeline.rs` |
| Regex patterns | `crates/screenpipe-redact/src/adapters/regex.rs:44-100` |
| VISION/принципы | `VISION.md`, `DESIGN.md`, `CLAUDE.md` |
