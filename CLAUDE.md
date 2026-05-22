# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

# Gilb (Gilbreth)

Desktop-приложение, которое записывает действия пользователя через accessibility API
(macOS + Windows; Linux вне scope). Cargo workspace + Tauri 2.

## Контекст и фокус

Архитектура из трёх слоёв:

1. **Сбор сырых a11y-данных** — **текущий фокус.**
2. Pattern mining (therbligs) — отложено.
3. Agent skill — отложено.

Дорожная карта по фазам, чек-лист готовности Layer 1 и решения,
зафиксированные с пользователем, лежат в:

- `spec.md` — целевая архитектура и расхождения между планами.
- `tauri-plan.md` — пофазовый план (Phase 0 → Phase 7 gate).
- `research/` — разбор reference-проектов и рекомендации по Layer 1
  (особенно `05-gilb-recommendations.md`, `06-layer1-capture-quality.md`).

Перед нетривиальными изменениями архитектуры сверяйся с этими тремя
файлами — в них уже зафиксированы решения, которые иначе придётся
переоткрывать.

## Команды

Сборка и запуск идут через корневой Cargo workspace + npm внутри
`apps/gilb-app-tauri`.

```sh
# Cargo workspace (Rust). Запускать из корня репо.
cargo build                                # вся workspace
cargo build -p gilb-a11y                   # один crate
cargo test                                 # все тесты
cargo test -p gilb-db                      # тесты одного crate
cargo test -p gilb-a11y text_buffer        # тесты с фильтром по имени
cargo clippy --workspace --all-targets     # lint
cargo fmt --all                            # format

# CLI smoke для Layer 1 (без Tauri UI).
cargo run -p gilb-a11y --bin gilb-a11y-cli -- --seconds 5
cargo run -p gilb-a11y --bin gilb-a11y-cli -- --db /tmp/gilb.sqlite --seconds 10

# Tauri (frontend + Rust shell). Запускать из apps/gilb-app-tauri.
cd apps/gilb-app-tauri
npm install                                # один раз
npm run tauri dev                          # dev shell (горячая перезагрузка фронта)
npm run tauri build                        # release .dmg/.msi с подписью из tauri.conf.json
```

Capture defaults управляются env vars из `RecordingSettings::from_env`:
`CAPTURE_EVENTS`, `CAPTURE_MOUSE_MOVE`, `CAPTURE_CLIPBOARD`,
`CAPTURE_TREE_SNAPSHOTS`. Логирование — `RUST_LOG=...` (по умолчанию
`info,gilb=debug` в Tauri shell, `info` в CLI).

База лежит в `~/.gilb/db.sqlite` (см. `gilb_config::db_path`); тот же путь
использует и Tauri-app, и CLI smoke, если не передан `--db`.

## Архитектура

### Crates и зависимости

```
gilb-core ──► (типы: Action, ActionKind, AppInfo, ElementContext, SessionId)
gilb-config ─► (RecordingSettings, data_dir / db_path)
gilb-events ─► (EventBus: broadcast PermissionEvent + HealthEvent)

gilb-db ─────► gilb-core, gilb-config
              (SqlitePool + миграции в migrations/, модули sessions/actions)

gilb-a11y ───► gilb-core, gilb-config, gilb-events, gilb-db
              (trait CapturePlatform; cfg-разделённые реализации;
               text_buffer, activity_feed, budget, tree/, password_masking;
               bin gilb-a11y-cli)

gilb-engine ─► все crates выше
              (Engine — длительный процесс-wide объект; владеет DB pool,
               EventBus, текущей CaptureSession; spawn'ит writer task)

apps/gilb-app-tauri/src-tauri ─► gilb-engine + gilb-config + gilb-events
              (Tauri commands: start_capture/stop_capture/status/
               open_privacy_pane; AppState держит Arc<Engine>)
```

Дополнительные дочерние линии:
`crates/gilb-a11y/src/platform/{macos,windows,unsupported}` — выбираются
через `cfg(target_os = ...)`. `macos/` сейчас разбит на под-модули
(`event_tap`, `ax_worker`, `focus`, `keyboard`, `pasteboard`, `normalizer`,
`permissions`, `ffi`, `platform`). `windows.rs` — заглушка до Phase 6.

### Поток данных capture → DB

1. UI (или CLI) дёргает `Engine::start_capture(settings)`.
2. `Engine` пишет строку в `sessions`, открывает mpsc-канал
   (`ACTION_CHANNEL_CAPACITY = 4096`), спавнит writer-таску и зовёт
   `CapturePlatform::start(StartContext { session_id, action_tx,
   event_bus, settings })`.
3. Платформенный capture (на macOS — CGEventTap + AX) кладёт
   `gilb_core::Action` в `action_tx`.
4. Writer-таска в `gilb-engine` вызывает `gilb_db::actions::insert_action`
   по одному (Phase 3 заменит на batched write queue).
5. `Engine::stop_capture` останавливает worker → отправляет shutdown в
   writer → закрывает `sessions` row с `stop_reason`.

Permission / health события идут параллельно через `EventBus`
(`tokio::sync::broadcast` каналы внутри `gilb-events`).

### База данных

`gilb-db::open_db` открывает SQLite с фиксированным набором PRAGMA
(WAL, `synchronous=NORMAL`, `cache_size=-65536`, `mmap_size=256MB`,
`busy_timeout=5s`, `wal_autocheckpoint=4000`) и применяет миграции из
`crates/gilb-db/migrations/`. v0 схема — `sessions`, `actions`,
`tree_snapshots`, `app_budgets`, `health_events` (см. `0001_init.sql` и
`spec.md §4`). Multimodal-таблицы (frames / elements / ocr_text /
audio_*) добавляются миграциями в Phase 8+, **не** заводятся заранее.

## Структура репо

- `Cargo.toml` — workspace root (members = `apps/gilb-app-tauri/src-tauri`
  + `crates/*`). Общие версии зависимостей — в `[workspace.dependencies]`,
  каждый crate подтягивает их через `workspace = true`.
- `plan.md` — старый план разбора prior-art (см. также `research/`).
- `tauri-plan.md` — текущий пофазовый план имплементации.
- `spec.md` — целевая архитектура Tauri-проекта.
- `research/` — research-документы (архитектурные обзоры, рекомендации,
  разбор reference-проектов).
- `reference/` — сторонние проекты, которые мы изучаем и из которых
  копируем подходы. **Не наш код**, **не коммитится**
  (см. `.gitignore`). Каждая подпапка обычно сама по себе git-репозиторий
  (клон upstream'а).
- `.zenflow/` — рабочее состояние zenflow (не коммитится).

## Работа с `reference/`

- `reference/` исключён из git. Обновление выполняется как обычный pull
  в соответствующем клоне: `cd reference/<project> && git pull`.
- Документы в `research/` могут ссылаться на пути внутри
  `reference/<project>/...` — это допустимо и ожидаемо.
- Если нужно скопировать кусок кода из reference в gilb — копируй явно
  в исходники gilb с указанием источника в commit message.

Текущие reference-проекты:

- `reference/prior-art` — Rust workspace + Tauri desktop app, источник
  подходов к захвату a11y / экрана / событий. Разбор см. в `research/`.

## macOS specifics

- Bundle ID: `app.farol.gilb`. Apple Developer Team ID: `83856566PM`.
  `signingIdentity` уже прописан в `apps/gilb-app-tauri/src-tauri/tauri.conf.json`.
- `Info.plist`: `LSUIElement=1` (без иконки в Dock),
  `NSAccessibilityUsageDescription` + `NSInputMonitoringUsageDescription`
  + `NSAppleEventsUsageDescription`.
- `entitlements.plist`: hardened runtime, `automation.apple-events`,
  `disable-library-validation` (нужно для AX-FFI). JIT / unsigned-exec
  выключены — не включай без необходимости.
- AX/Input Monitoring permission даются пользователем в System Settings;
  команда `open_privacy_pane` в `lib.rs` открывает соответствующий pane
  через `x-apple.systempreferences:` URL.
- macOS-only crates подключаются через
  `[target.'cfg(target_os = "macos")'.dependencies]` в `gilb-a11y`
  (`core-graphics`, `core-foundation`, `accessibility-sys`, `objc2*`).
