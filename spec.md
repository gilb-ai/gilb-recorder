# Spec: gilb — Tauri desktop app, Layer 1 a11y capture

Документ синтезирует два источника:

1. **Research-заметки** в `/Users/leonid/src/gilb/reference/research/` (особенно `05-gilb-recommendations.md`, `06-layer1-capture-quality.md`) — описывают 3-слойную архитектуру с фокусом на Layer 1 (pure a11y stream), multi-crate workspace, минимальную схему БД.
2. **План пользователя** "gilb — Tauri desktop app c full-spec БД как у screenpipe" — описывает один Tauri проект + crate `native/capture-core`, полную multimodal схему БД (frames + elements + ocr_text + ui_events + audio + meetings + memories), фазы 0–8.

Между источниками есть существенные расхождения; ниже — разбор и предлагаемый синтез.

## 1. Контекст

- Платформы: **macOS + Windows** (Linux вне scope). macOS-first, Windows phase ≥ 6.
- Bundle ID: `app.farol.gilb`.
- Текущий фокус — **Layer 1**: устойчивый, дедуплицированный, ordered поток действий пользователя через a11y. Layers 2 (pattern mining) и 3 (agent skill) — вне scope этой итерации.
- Хранилище: локально, `~/.gilb/db.sqlite` + `~/.gilb/snapshots/` (когда дойдём до снэпшотов).
- Retention: actions — forever; tree_snapshots.root_json — soft-evict (детали в research §2.2 "Soft-evict policy"); cloud sync — отложено.

## 2. Расхождения и предлагаемое разрешение

| Аспект | План пользователя | Research | Предлагаемое разрешение |
|---|---|---|---|
| Структура workspace | один Tauri + `native/capture-core` (lib+bin) | multi-crate: `crates/gilb-a11y, gilb-db, gilb-engine, gilb-core, gilb-config, gilb-events` + `apps/gilb-app-tauri` | **Multi-crate как в research.** Разделение даёт чёткие границы, переиспользование (CLI smoke binary остаётся), и совпадает с уже принятой архитектурой в памяти проекта. |
| Схема БД v0 | full multimodal (frames + elements + ocr_text + ui_events + audio_chunks + speakers + meetings + memories + video_chunks + FTS5 на всём) | минимальная: `sessions, actions, tree_snapshots, app_budgets, health_events` + `actions_fts` | **Минимальная как в research.** Layer 1 пишет действия и контекст; multimodal таблицы добавим миграциями, когда дойдут реальные потребители (Layer 2 / OCR / audio). Это исключает мёртвый schema-overhead. |
| Pure a11y vs multimodal в v0 | план описывает в phase 0–1 только a11y, но schema готова под всё | "v0 Layer 1 — pure a11y stream", snapshot / OCR / video / audio — позже, отдельными миграциями | **Pure a11y stream.** Snapshot/OCR/audio добавляем когда чек-лист 06-layer1-capture-quality §5 закрыт. |
| Расположение Tauri-app | `gilb/src-tauri` (Tauri как корень) | `apps/gilb-app-tauri/src-tauri` (Tauri как app внутри cargo workspace) | **`apps/gilb-app-tauri`** — корень workspace это `gilb/`, Tauri-проект внутри `apps/`, crates параллельно в `crates/`. |
| Переименования | `git mv native/rodnik-capture native/capture-core`, `rodnik-ov-poc → ov-poc` | (не упоминаются) | **Не применимо в этом worktree.** Текущий `/Users/leonid/src/gilb/` не содержит ни `native/rodnik-capture`, ни `rodnik-ov-poc`. Стартуем с чистого листа. Переименования из плана пользователя относятся к репо `/Users/leonid/src/context/` (другой контекст), их пропускаем. |
| Workspace path | `/Users/leonid/src/context/` | `/Users/leonid/src/gilb/` | **`/Users/leonid/src/gilb/`** — это активный worktree. |
| Defaults | `CAPTURE_EVENTS=1`, `CAPTURE_MOUSE_MOVE=0` | adaptive FPS + per-app WalkBudget из коробки | **Обе.** Defaults пользователя оставляем; adaptive FPS + WalkBudget — обязательны (research §2.1, §2.2). |
| Password masking | AXSecureTextField + name patterns + bg worker для element-at-position | AXSecureTextField + heuristic + excluded apps list (1Password, Bitwarden, ...) + regex PII | **Объединение:** AXSecureTextField + name patterns + excluded apps + regex PII redactor + bg worker для `AXUIElementCopyElementAtPosition` через bounded(4) channel и `AX_QUERY_LOCK`. |
| FTS5 external content | full multimodal: frames_fts, elements_fts, ui_events_fts, audio_transcriptions_fts, memories_fts | один `actions_fts` (external content поверх `actions`) | **`actions_fts` в v0.** Остальные FTS-индексы добавляем вместе с соответствующими таблицами в более поздних фазах. Триггеры — синхронные (research §4.3: deferred indexer screenpipe откатил из-за latency). |

## 3. Целевая архитектура

```
/Users/leonid/src/gilb/
├── Cargo.toml                            # workspace root
├── apps/
│   └── gilb-app-tauri/
│       ├── package.json
│       ├── src/                          # web UI (vanilla TS)
│       └── src-tauri/
│           ├── Cargo.toml
│           ├── tauri.conf.json           # bundle.identifier = app.farol.gilb
│           ├── Info.plist                # LSUIElement + Accessibility/Input usage
│           └── src/main.rs               # tauri commands → gilb-engine
├── crates/
│   ├── gilb-core/                        # shared types: Action, ElementContext, AppInfo
│   ├── gilb-config/                      # RecordingSettings (CLI + UI shared)
│   ├── gilb-events/                      # broadcast pub/sub (permission/health)
│   ├── gilb-a11y/
│   │   ├── src/lib.rs
│   │   ├── src/platform/
│   │   │   ├── mod.rs                    # trait CapturePlatform
│   │   │   ├── macos.rs                  # CGEventTap + AX + observer
│   │   │   ├── windows.rs                # stub в phase 0, реализация phase 6
│   │   │   └── unsupported.rs            # fallback
│   │   ├── src/text_buffer.rs            # 300ms debounce aggregator
│   │   ├── src/activity_feed.rs          # adaptive FPS
│   │   ├── src/budget.rs                 # per-app WalkBudget
│   │   ├── src/tree/cache.rs             # SimHash dedup
│   │   └── src/bin/gilb-a11y-cli.rs      # standalone smoke binary
│   ├── gilb-db/
│   │   ├── src/db.rs                     # open/migrate/PRAGMA WAL, ImmediateTx, pools
│   │   ├── src/write_queue.rs            # batch ≤500 ops/TX
│   │   ├── src/actions.rs                # INSERT actions
│   │   ├── src/trees.rs                  # INSERT tree_snapshots
│   │   ├── src/search.rs                 # FTS5 query helpers
│   │   └── migrations/
│   │       ├── 0001_init.sql             # sessions+actions+tree_snapshots+app_budgets+health_events
│   │       └── 0002_actions_fts.sql      # FTS5 external content + triggers
│   └── gilb-engine/                      # capture session lifecycle, ServerCore split
└── docs/                                 # vision/architecture/decisions (создаём по ходу)
```

## 4. Схема БД v0 (минимальная)

PRAGMA: `journal_mode=WAL`, `synchronous=NORMAL`, `cache_size=-65536`, `mmap_size=268435456`, `temp_store=MEMORY`, `wal_autocheckpoint=4000`, `busy_timeout=5000`, `foreign_keys=ON`.

Таблицы — из research `05-gilb-recommendations.md §B.1`:

- `sessions` — границы записи (start/stop, gilb_version, stop_reason).
- `actions` — атомарное действие (kind, app, window, element context, password-flag, link to tree_snapshot).
- `tree_snapshots` — periodic AX walks, dedupted SimHash'ем.
- `app_budgets` — persistent throttling per-app.
- `health_events` — permission/wake/crash/drop события.
- `actions_fts` — FTS5 external content поверх `actions(text_content, element_name, window_title, app_name)` + AI/AU/AD триггеры строго по паттерну screenpipe `20260415000000_frames_fts_external_content.sql`.

**Расширения позже (миграциями):**
- `tree_snapshots.root_json` evict policy.
- `frames` + `elements` + `ocr_text` (когда добавим snapshot/OCR pipeline).
- `ui_events` если решим разделить с `actions` (или оставим единый `actions`).
- `audio_chunks` / `audio_transcriptions` / `speakers` (когда дойдём до audio).
- `meetings`, `memories` — соответствующие фичи.
- `sync_id` / `cloud_blob_id` колонки ALTER'ом для cloud sync.

## 5. Phasing (синтез)

Каждая фаза — одна логическая единица работы (≈ один PR / коммит).

### Phase 0 — Foundation (workspace + Tauri skeleton + минимальная БД)

- Cargo workspace root `/Users/leonid/src/gilb/Cargo.toml` с членами `apps/gilb-app-tauri/src-tauri` + `crates/*`.
- `npm create tauri-app@latest apps/gilb-app-tauri -- --template vanilla-ts`.
- `tauri.conf.json`: bundle.identifier=`app.farol.gilb`, macOS minimumVersion=`13.0`, entitlements (Accessibility, Input Monitoring); Info.plist: `LSUIElement=1`, `NSAccessibilityUsageDescription`, `NSInputMonitoringUsageDescription`.
- Skeleton crates: `gilb-core`, `gilb-config`, `gilb-events`, `gilb-a11y` (с trait + cfg-gated platform/{macos,windows,unsupported}), `gilb-db`, `gilb-engine`.
- `gilb-db`: open/migrate + PRAGMA WAL tuning + миграция `0001_init.sql` со схемой §4.
- Tauri commands: `start_capture`, `stop_capture`, `status`. UI: status checklist (AX granted / Input Monitoring granted), счётчик actions in DB, Start/Stop кнопки.
- `gilb-a11y::platform::macos::start_capture` — пустой stub, пишет одну тестовую `actions` row при старте, чтобы доказать сквозной путь.
- `gilb-a11y` standalone bin (CLI smoke), как в плане пользователя.
- Acceptance: `npm run tauri build` → `.dmg` → install → grant AX → нажать Start → `sqlite3 ~/.gilb/db.sqlite "select * from sessions; select * from actions;"` показывает rows. `cargo run -p gilb-a11y --bin gilb-a11y-cli` тоже работает.

### Phase 1 — Event capture macOS (clicks, keys, text, scroll, clipboard)

- `gilb-a11y/src/platform/macos.rs`: CGEventTap LISTEN_ONLY через `cidre`; IOHIDCheckAccess для permission check.
- TextBuffer 300мс debounce + UCKeyTranslate (с deadkey state на RU/EN — прототип, см. open question 1).
- Clipboard poller 750мс через `NSPasteboard.changeCount()`.
- `current_focused_role` ArcSwap для маскирования text-flush'ей в password-полях.
- Password masking: AXSecureTextField + name patterns + hardcoded excluded apps (1Password, Bitwarden, KeePassXC, Keychain Access).
- ax-worker thread с bounded(4) channel для `AXUIElementCopyElementAtPosition` на click.
- `AX_QUERY_LOCK` mutex try-lock — пропускаем context на занятость, не пропускаем сам event.
- Defaults: `CAPTURE_EVENTS=1`, `CAPTURE_MOUSE_MOVE=0`.
- Unit tests: TextBuffer flush, is_password_field detection.
- Acceptance: печать в Notes → `text` actions с правильным `text_content`; печать в Safari password → `[masked]`; click на кнопку → `element_role`/`element_name`/`element_value` заполнены; 1Password → ничего.

### Phase 2 — Tree capture + adaptive FPS + per-app budget

- `walk_focused_window` через AX + AX observer на focused pid (reattach на app switch).
- Per-app `WalkBudget` (Light/Moderate/Heavy/Critical) с persisted state в `app_budgets`.
- SimHash dedup `tree_snapshots` (hamming > 10 bits OR TTL 60s).
- Adaptive FPS из `activity_feed.rs` (5 Hz active typing → 1 Hz idle → 0.5 Hz deep idle).
- Metric collection: events/min, tree_walks/min, % snapshots stored vs walked.
- Acceptance: Discord/Slack/VS Code автоматически переходят в Critical tier; скроллинг docs не плодит дубль tree_snapshots; CPU avg < 5% в idle.

### Phase 3 — Robustness (write queue, sleep, permission, recovery)

- `gilb-db::write_queue` — batch ≤500 ops/TX, BEGIN IMMEDIATE, prepared statements, single retry on error.
- `ImmediateTx` wrapper + split read/write connection pool + single-permit write semaphore.
- Bounded channels везде с drop-with-warn.
- Sleep monitor (CFNotification) → invalidation flag → stream re-create.
- Permission revoke detection → `PermissionEvent::Lost` через `gilb-events` → reduced mode без падения; `Restored` → reattach.
- WAL checkpoint discipline: `wal_autocheckpoint=4000` + `wal_checkpoint(TRUNCATE)` при старте.
- `gilb db recover` CLI: VACUUM → REINDEX → ANALYZE → integrity_check + foreign_key_check.
- Migration checksum self-heal (паттерн screenpipe `db.rs:477-495`).
- Acceptance: 8h работы — БД растёт линейно, нет deadlock; убил процесс по `kill -9` → recover CLI восстановил БД.

### Phase 4 — UI polish, tray, pause, ServerCore split

- Tray icon: `recording` / `paused` / `unhealthy`; меню Pause/Resume/Quit.
- Global pause через `gilb-events`.
- Per-app exclusion list в `config.toml` + UI.
- Permission status checklist в UI.
- "Сегодня записано N actions" view.
- ServerCore vs CaptureSession разделение — pause/resume без рестарта capture session.
- Acceptance: Pause из tray останавливает запись < 100мс; Resume — за < 200мс без потери session_id.

### Phase 5 — FTS5 search demo

- `0002_actions_fts.sql`: `actions_fts` virtual table + AI/AU/AD triggers (external content).
- `gilb-db::search` helpers с MATCH-query API.
- UI: search demo (поле ввода → результаты с timestamp + app + window).
- Sanity check FTS5 на RU корпусе: «АмоCRM», «Контур», «согласовать» — query < 50мс на 100K rows.
- Acceptance: 100K actions, FTS5 query даёт результаты < 50мс.

### Phase 6 — Windows backend

- `crates/gilb-a11y/src/platform/windows.rs`: SetWindowsHookEx (WH_MOUSE_LL + WH_KEYBOARD_LL) на отдельном потоке.
- UIA через `windows@0.58` на apartment-threaded worker'е с CacheRequest batching.
- Control View + TreeWalker fallback для Chromium/Electron.
- `IUIAutomationFocusChangedEventHandler` для focus tracking.
- CI build matrix добавляет windows-latest (cargo build + cargo test).
- Tauri `.msi` installer.
- Acceptance: тот же стресс-тест 8h на Windows — те же CPU/RAM/disk budgets.

### Phase 7 — Layer 1 gate (стресс-тест + чек-лист)

- Прогон `06-layer1-capture-quality.md §5` чек-листа на обеих платформах.
- 8 часов реальной работы: CPU avg < 5%, RAM steady-state < 500 MB, БД растёт линейно.
- Документация: `docs/architecture.md` (картинка + Layer 1 entry point), `docs/decisions.md` (Tauri shell, SQLite multi-table eventual, FTS5 unicode61 external content, retention forever, ServerCore split), `docs/vision.md` (статус слоёв), `CLAUDE.md` (orientation).
- Gate: после закрытия — переходим к Layer 2 (вне scope этой итерации).

### Phases 8+ — отложено (Layer 2 и далее)

- Phase 8: Screen snapshots (event-driven, debounce 500мс, ScreenCaptureKit / Windows Graphics Capture, FrameLinker actor) + миграция `frames`/`elements`/`ocr_text`.
- Phase 9: Audio + meetings + speakers + sqlite-vec + ASR ingestion.
- Phase 10: Memories + RAG.
- Phase 11: Cloud sync (sync_id ALTER, upload worker, retention policy для cloud users).

## 6. Cargo dependencies (MVP)

```toml
[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
sqlx = { version = "0.7", features = ["sqlite", "runtime-tokio-rustls", "migrate"] }
rusqlite = { version = "0.31", features = ["bundled", "modern_sqlite"] }
arc-swap = "1"
parking_lot = "0.12"
crossbeam-channel = "0.5"
dashmap = "5"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
chrono = "0.4"
uuid = { version = "1", features = ["v4"] }
arboard = "3"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tracing-appender = "0.2"
anyhow = "1"
thiserror = "1"

[target.'cfg(target_os = "macos")'.dependencies]
cidre = { version = "0.13", features = ["ax", "cg", "blocks", "ns", "dispatch"] }
tracing-oslog = "0.2"

[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "0.58", features = [
    "Win32_Foundation", "Win32_UI_Accessibility",
    "Win32_UI_WindowsAndMessaging", "Win32_System_Com", "Win32_System_Ole",
] }
```

`tauri = "2"` + `tauri-plugin-tray = "2"` — внутри `apps/gilb-app-tauri/src-tauri/Cargo.toml`.

`sqlite-vec` — только когда дойдём до speaker embeddings (phase 9+).

## 7. Зафиксированные решения и оставшиеся технические проверки

### Зафиксировано пользователем

| Вопрос | Решение |
|---|---|
| Структура workspace | **multi-crate** (`crates/gilb-*` + `apps/gilb-app-tauri`) |
| Схема БД v0 | **минимальная**; multimodal — миграциями позже |
| БД-слой | **sqlx@0.7** (sqlite + runtime-tokio-rustls + migrate) |
| UI framework | **vanilla TS** |
| Apple Developer team ID (macOS code signing) | **`83856566PM`** |
| Worktree path | **`/Users/leonid/src/gilb/`** (переименования из исходного плана пропускаем) |
| Bundle ID | `app.farol.gilb` |
| Tauri-app расположение | `apps/gilb-app-tauri` |
| `actions` vs разделение на `ui_events`/`frames`/`elements` | **единая `actions`** в v0 |

### Технические проверки по ходу фаз (не блокируют старт)

1. **UCKeyTranslate с deadkey state на RU/EN раскладках** — phase 1, прототип-обёртка над `UCKeyTranslate` + persistent dead state.
2. **AX query throughput при click-burst на Chrome/Figma** — phase 2: measure p99; при p99 > 500мс увеличить bounded channel до 8 или drop-on-full.
3. **WAL concurrent read** — phase 3 sanity check: `sqlite3 cli SELECT` во время writer'а не должен блокировать.
4. **NSWorkspace observer на non-main thread** — phase 2 потенциальный блокер; fallback — Cocoa main-thread runloop.
5. **sqlx + sqlite bundled universal binary (Intel+ARM)** — phase 0 проверить (`libsqlite3-sys` features).
6. **FTS5 на больших корпусах русского** — phase 5: «АмоCRM», «Контур», «согласовать» < 50мс на 100K rows.
7. **`tree_snapshots` со 100K-300K rows/день** — phase 2 INSERT performance ≥ 300 rows/сек.

## 8. Verification (phase 0 acceptance)

```sh
cd /Users/leonid/src/gilb
cargo build --workspace
cd apps/gilb-app-tauri
npm install
npm run tauri dev          # окно открывается
npm run tauri build        # .dmg → src-tauri/target/release/bundle/dmg/
# Установить .app, grant AX permission, нажать Start
sqlite3 "$HOME/.gilb/db.sqlite" "
  SELECT COUNT(*) FROM sessions;
  SELECT COUNT(*) FROM actions;
  SELECT kind, COUNT(*) FROM actions GROUP BY kind;
"
```

## 9. Critical files (создаваемые в phase 0)

- `/Users/leonid/src/gilb/Cargo.toml` (workspace root)
- `/Users/leonid/src/gilb/apps/gilb-app-tauri/` (Tauri 2 проект целиком)
- `/Users/leonid/src/gilb/apps/gilb-app-tauri/src-tauri/tauri.conf.json`
- `/Users/leonid/src/gilb/apps/gilb-app-tauri/src-tauri/Info.plist`
- `/Users/leonid/src/gilb/apps/gilb-app-tauri/src-tauri/src/main.rs`
- `/Users/leonid/src/gilb/crates/gilb-{core,config,events,a11y,db,engine}/`
- `/Users/leonid/src/gilb/crates/gilb-db/migrations/0001_init.sql`
- `/Users/leonid/src/gilb/docs/{architecture,decisions,vision}.md` (постепенно)
- `/Users/leonid/src/gilb/CLAUDE.md`
- `/Users/leonid/src/gilb/.gitignore` (target/, node_modules/, dist/, *.log, *.dmg)
