# Plan: gilb — Tauri desktop app, Layer 1 a11y capture

Полная спецификация: см. `spec.md` в этом же каталоге. Здесь — пошаговый план для имплементации.

## Источники

- Research-заметки: `/Users/leonid/src/gilb/reference/research/` — 3-слойная архитектура, фокус Layer 1 (pure a11y stream), multi-crate workspace, минимальная схема БД.
- План пользователя "gilb — Tauri desktop app c full-spec БД как у screenpipe" — Tauri shell, bundle ID `app.farol.gilb`, defaults `CAPTURE_EVENTS=1` / `CAPTURE_MOUSE_MOVE=0`.
- Синтез в `spec.md`: multi-crate workspace + Tauri-app в `apps/gilb-app-tauri`, минимальная схема БД v0 (расширяем миграциями), pure a11y stream в v0, snapshot/OCR/audio — отложены до закрытия Layer 1 gate.

## Зафиксированные решения (подтверждено пользователем)

- **Workspace**: multi-crate (`crates/gilb-a11y, gilb-db, gilb-engine, gilb-core, gilb-config, gilb-events` + `apps/gilb-app-tauri`).
- **Схема БД v0**: минимальная (`sessions, actions, tree_snapshots, app_budgets, health_events` + `actions_fts`). Multimodal (frames/elements/ocr_text/audio/...) — миграциями позже.
- **БД-слой**: `sqlx@0.7` (sqlite + runtime-tokio-rustls + migrate).
- **UI framework**: vanilla TS.
- **macOS Apple Developer team ID**: `83856566PM` (для code signing в Phase 0 / Phase 7).
- **Worktree path**: `/Users/leonid/src/gilb/`. Переименования `native/rodnik-capture` / `rodnik-ov-poc` из исходного плана пользователя пропускаем — этих папок в репо нет.
- **Bundle ID**: `app.farol.gilb`.
- **Pure a11y stream в v0**: snapshot/OCR/audio/video — после закрытия Layer 1 gate (Phase 7+).

Остальные технические open questions (deadkey state, AX throughput, rusqlite universal binary, FTS5 RU sanity check) — проверяем по ходу соответствующих фаз; см. `spec.md §7`.

## Implementation steps

### [ ] Step: Phase 0 — Foundation (workspace + Tauri skeleton + minimal DB)

- Cargo workspace root `/Users/leonid/src/gilb/Cargo.toml` с членами `apps/gilb-app-tauri/src-tauri` + `crates/*`.
- `.gitignore`: `target/`, `node_modules/`, `dist/`, `*.log`, `*.dmg`, `.DS_Store`.
- `npm create tauri-app@latest apps/gilb-app-tauri -- --template vanilla-ts`.
- `tauri.conf.json`: bundle.identifier=`app.farol.gilb`, macOS minimumVersion=`13.0`, entitlements (Accessibility, Input Monitoring); Info.plist: `LSUIElement=1`, `NSAccessibilityUsageDescription`, `NSInputMonitoringUsageDescription`.
- Skeleton crates: `gilb-core`, `gilb-config`, `gilb-events`, `gilb-a11y` (с trait `CapturePlatform` + cfg-gated platform/{macos,windows,unsupported}), `gilb-db`, `gilb-engine`.
- `gilb-db`: open/migrate + PRAGMA WAL tuning (WAL, synchronous=NORMAL, mmap_size=256MB, cache_size=64MB, busy_timeout=5s, wal_autocheckpoint=4000).
- Миграция `0001_init.sql`: таблицы `sessions, actions, tree_snapshots, app_budgets, health_events` + индексы из `spec.md §4`.
- Tauri commands: `start_capture`, `stop_capture`, `status`. UI: status checklist (AX granted / Input Monitoring granted), счётчик actions in DB, Start/Stop кнопки.
- `gilb-a11y::platform::macos::start_capture` — stub, пишет одну тестовую `actions` row при старте, доказательство сквозного пути.
- `gilb-a11y` standalone bin (`gilb-a11y-cli`) — CLI smoke.
- Acceptance: `npm run tauri build` → `.dmg` → install → grant AX → нажать Start → `sqlite3 ~/.gilb/db.sqlite` показывает rows. CLI smoke тоже работает.

### [ ] Step: Phase 1 — Event capture macOS (clicks, keys, text, scroll, clipboard)

- `gilb-a11y/src/platform/macos.rs`: CGEventTap LISTEN_ONLY через `cidre`; IOHIDCheckAccess для permission.
- `text_buffer.rs`: 300мс debounce aggregator + UCKeyTranslate с deadkey state на RU/EN.
- Clipboard poller 750мс через `NSPasteboard.changeCount()`.
- `current_focused_role` ArcSwap — маскирование text-flush в password-полях.
- Password masking: AXSecureTextField + name patterns + hardcoded excluded apps (1Password, Bitwarden, KeePassXC, Keychain Access) + regex PII redactor.
- ax-worker thread с bounded(4) channel для `AXUIElementCopyElementAtPosition` на click.
- `AX_QUERY_LOCK` mutex try-lock — пропускаем context на занятость, не пропускаем event.
- Defaults: `CAPTURE_EVENTS=1`, `CAPTURE_MOUSE_MOVE=0`.
- Unit tests: TextBuffer flush, is_password_field, excluded apps list.
- Acceptance: печать в Notes → `text` actions с правильным `text_content`; печать в Safari password → `[masked]`; click на кнопку → `element_role`/`element_name`/`element_value` заполнены; 1Password → ничего.

### [ ] Step: Phase 2 — Tree capture + adaptive FPS + per-app WalkBudget + SimHash dedup

- `walk_focused_window` через AX + AX observer на focused pid (reattach на app switch).
- `budget.rs`: per-app `WalkBudget` (Light/Moderate/Heavy/Critical tiers) + persisted state в `app_budgets`.
- `tree/cache.rs`: SimHash dedup `tree_snapshots` (hamming > 10 bits OR TTL 60s).
- `activity_feed.rs`: adaptive FPS (5 Hz active → 1 Hz idle → 0.5 Hz deep idle).
- Metrics: events/min, tree_walks/min, % snapshots stored vs walked.
- Acceptance: Discord/Slack/VS Code автоматически Critical; скроллинг docs не плодит дубль snapshots; CPU avg < 5% idle.

### [ ] Step: Phase 3 — Robustness (write queue, sleep, permission, recovery)

- `gilb-db::write_queue`: batch ≤500 ops/TX, BEGIN IMMEDIATE, prepared statements, single retry on error.
- `ImmediateTx` wrapper + split read/write connection pool + single-permit write semaphore.
- Bounded channels everywhere с drop-with-warn.
- Sleep monitor (CFNotification) → invalidation flag → stream re-create.
- Permission revoke/restore detection → `gilb-events` publish → reduced mode без падения.
- WAL checkpoint discipline (`wal_checkpoint(TRUNCATE)` при старте).
- `gilb db recover` CLI: VACUUM → REINDEX → ANALYZE → integrity_check + foreign_key_check.
- Migration checksum self-heal.
- Acceptance: 8h работы — БД линейно растёт, нет deadlock; `kill -9` → recover CLI восстановил БД.

### [ ] Step: Phase 4 — UI polish, tray, global pause, ServerCore split

- Tray icon: `recording` / `paused` / `unhealthy`; меню Pause/Resume/Quit.
- Global pause через `gilb-events`.
- Per-app exclusion list в `config.toml` + UI.
- Permission status checklist в UI.
- "Сегодня записано N actions" view.
- ServerCore vs CaptureSession разделение — pause/resume без рестарта capture session.
- Acceptance: Pause из tray останавливает запись < 100мс; Resume < 200мс без потери session_id.

### [ ] Step: Phase 5 — FTS5 search demo (actions_fts)

- Миграция `0002_actions_fts.sql`: `actions_fts` virtual table (unicode61, external content) + AI/AU/AD triggers по паттерну screenpipe `20260415000000_frames_fts_external_content.sql`.
- `gilb-db::search` helpers с MATCH-query API.
- UI: search demo (поле ввода → результаты с timestamp + app + window).
- Sanity check FTS5 на RU корпусе: «АмоCRM», «Контур», «согласовать» < 50мс на 100K rows.
- Acceptance: 100K actions, FTS5 query < 50мс.

### [ ] Step: Phase 6 — Windows backend

- `crates/gilb-a11y/src/platform/windows.rs`: SetWindowsHookEx (WH_MOUSE_LL + WH_KEYBOARD_LL) на отдельном потоке.
- UIA через `windows@0.58` на apartment-threaded worker'е с CacheRequest batching.
- Control View + TreeWalker fallback для Chromium/Electron.
- `IUIAutomationFocusChangedEventHandler` для focus tracking.
- CI build matrix — windows-latest (cargo build + cargo test).
- Tauri `.msi` installer.
- Acceptance: тот же стресс-тест 8h на Windows — те же CPU/RAM/disk budgets.

### [ ] Step: Phase 7 — Layer 1 gate (стресс-тест + чек-лист + docs)

- Прогон `06-layer1-capture-quality.md §5` чек-листа на macOS + Windows.
- 8 часов реальной работы: CPU avg < 5%, RAM steady-state < 500 MB, БД линейно.
- Документация: `docs/architecture.md` (картинка + Layer 1 entry), `docs/decisions.md` (D-records: Tauri shell, SQLite minimal schema + миграции, FTS5 unicode61 external content, retention forever for actions / soft-evict tree, ServerCore split), `docs/vision.md` (статус слоёв), `CLAUDE.md`.
- Gate: после закрытия — Layer 2 (вне scope этой итерации).

### [ ] Step: Phases 8+ — отложено (Layer 2 и далее)

Не реализуем сейчас, перечислены для полноты карты:
- Phase 8: Screen snapshots event-driven (ScreenCaptureKit / Windows Graphics Capture) + миграция `frames`/`elements`/`ocr_text` + FrameLinker actor.
- Phase 9: Audio + meetings + speakers + sqlite-vec + ASR ingestion.
- Phase 10: Memories + RAG.
- Phase 11: Cloud sync (sync_id ALTER, upload worker, retention policy).
