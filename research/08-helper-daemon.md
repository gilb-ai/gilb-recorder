# Helper-daemon: разбор архитектуры и рекомендация по адопции

Источник задачи — карточка [GILB-17](https://trello.com/c/hbthS1zy)
(split из [GILB-4](https://trello.com/c/D9Nf3Nqy)). Документ синтезирует
три входа:

1. Развёрнутое описание архитектуры в desc'е [GILB-4] — главный
   process + privileged helper + XPC/Unix-socket + SQLite single-writer
   + MCP внутри helper.
2. [GILB-1](https://trello.com/c/GoBLivUQ) — разбор того, почему TCC
   ключует разрешения по `(Bundle ID + Designated Requirement)` и при
   каких обновлениях grant слетает.
3. Наблюдаемое поведение reference-приложений: Karabiner-Elements
   (отдельный privileged daemon `karabiner_grabber`), Raycast и
   BetterTouchTool (стабильная подпись без helper'а),
   Hammerspoon (single-app + миграционный helper).

Документ заканчивается **yes/no с рисками** для адопции этой
архитектуры в gilb и уточнённым phase plan для сабкарточек
[GILB-18]..[GILB-21]. Версии крейтов не пинятся — это делается в карточке
имплементации.

Все даты обновлений крейтов сверены через `crates.io/api/v1/crates/...`
по состоянию на **2026-05-23**. Stale (>12 месяцев) явно помечены.

## 1. TCC mechanics — почему permission слетает на апдейте

TCC (Transparency, Consent, Control) — это пользовательский БД
`~/Library/Application Support/com.apple.TCC/TCC.db`, где Apple хранит
гранты. Ключ записи — кортеж **(client_type, client, service)**:

- `service` — тип разрешения (`kTCCServiceAccessibility`,
  `kTCCServiceListenEvent`, `kTCCServiceScreenCapture`, …).
- `client_type` — `0` для Bundle ID, `1` для абсолютного пути.
- `client` — Bundle ID или путь.

Кроме ключа, TCC сохраняет **csreq blob** — сериализованное
"Designated Requirement" из подписи бинаря на момент grant'а. При
каждом доступе TCC сверяет текущую подпись вызывающего процесса с
сохранённым csreq. Если требование больше не выполняется — grant
аннулируется и пользователь снова видит диалог.

Что меняет csreq → убивает grant:

| Триггер | Меняет csreq? |
|---|---|
| Смена Team ID | да |
| Смена certificate identity (rotated dev cert) | да |
| Смена Bundle ID (в т.ч. case) | да |
| ad-hoc подпись (CI без полной нотаризации) | да (csreq не resolve'ится) |
| `tauri-bundler` пересоздаёт `.app` с тем же Team ID и Bundle ID | **нет** — csreq стабилен |
| Изменение содержимого бинаря при том же DR | нет |
| Обновление macOS (включая major) | обычно нет, но известны regressions на 15.0 / 15.1 |

Что НЕ меняет csreq, но всё равно ломает grant:
- Helper-бинарь внутри `.app` бандла подписан с другим DR, чем main.
  TCC видит запрос от helper'а и просит отдельный grant.
- Quarantine reset (`xattr -d com.apple.quarantine`) после ручной
  переустановки — это не меняет csreq, но `tccutil reset Accessibility`
  пользователь иногда делает руками, чтобы починить совсем другое
  приложение, и сносит всё.

**Ключевой вывод для gilb.** Если main app `app.farol.gilb` сохраняет
тот же Bundle ID и подписан тем же Team ID `83856566PM` (`CLAUDE.md`
§ macOS specifics), csreq стабилен — теоретически grant переживает
обычные апдейты через auto-updater.

**Но** на практике есть две failure mode'ы, которые helper решает по
дизайну:

1. **Replace-on-update.** Tauri-обновление пересоздаёт `.app` бандл
   целиком (rename `.app.new` → `.app`). На некоторых версиях macOS
   TCC помечает запись stale пока процесс не запустится с правильной
   подписью; до перезапуска а11y-разрешение временно "молчит" (нет
   диалога, но `AXIsProcessTrustedWithOptions` возвращает false).
2. **Bundle ID future-proofing.** Если бизнес решит ребрендить с
   `app.farol.gilb` на `app.gilb` или сменить Team ID — без helper'а
   весь grant теряется. С helper'ом (frozen `ai.gilb.helper`) main
   app можно перебилдить с любым ID, helper остаётся.

Reference-наблюдение: Karabiner-Elements решил эту проблему
радикально — у них `karabiner_grabber` (демон, держащий Input
Monitoring + Accessibility) подписан годами одним и тем же DR. GUI
`Karabiner-Elements.app` обновляется свободно. Раз дав разрешение
grabber'у, пользователь не возвращается в System Settings. Raycast и
BetterTouchTool обходятся без helper'а, но **никогда** не меняли
свой Team ID и Bundle ID — это уже не "архитектурный паттерн",
а "не трогай ничего".

## 2. Process model — main + helper

Целевая раскладка из [GILB-4]:

```
gilb.app  (main, обновляется auto-updater'ом)
├── WebView (vanilla TS UI — Inbox, settings, ratification)
├── Tauri Rust shell
│   ├── читает hot.sqlite в read-only через WAL
│   ├── шлёт команды helper'у через IPC
│   └── НЕ держит permissions
└── Contents/Library/LaunchAgents/ai.gilb.helper.plist
    └── Contents/Resources/helpers/gilb-helper
                                    ↓
                        gilb-helper (privileged, Bundle ID frozen)
                        ├── AX capture (CGEventTap + AXObserver)
                        ├── ScreenCaptureKit (Layer 2+)
                        ├── CoreAudio + Whisper (Layer 2+)
                        ├── единственный SQLite writer
                        ├── PrefixSpan miner (Layer 2)
                        └── MCP server (stdio + socket)
```

**Что меняется относительно текущего workspace:**

- `crates/gilb-engine` сегодня живёт в Tauri shell (`apps/gilb-app-tauri/
  src-tauri`), spawn'ит writer task внутри Tauri process'а.
  В helper-архитектуре `gilb-engine` переезжает в новый
  `crates/gilb-helper` — он становится "телом" daemon-binary.
- `gilb-a11y` остаётся библиотекой; её зависимости не меняются.
  Главное — кто её вызывает (раньше Tauri main, теперь helper).
- `crates/gilb-db` разделяется на два режима использования: writer
  (helper открывает `OpenFlags::READ_WRITE | CREATE`) и reader
  (Tauri открывает `READ_ONLY`).
- `apps/gilb-mcp` (сегодня отдельный бинарь, см. `apps/gilb-mcp/Cargo.toml`)
  переезжает в helper как режим запуска (`gilb-helper --mcp-stdio`).
  Это устраняет дублирование SQLite-pool'ов и connection setup'а.
- `apps/gilb-app-tauri` теряет `gilb-engine` из зависимостей и получает
  тонкий IPC-клиент к helper'у (`start_capture` / `stop_capture` /
  `status` идут не в `Engine::start_capture(...)`, а в `helper_client::
  send(Command::StartCapture)`).

**Что НЕ меняется:**

- Crate boundaries `gilb-core`, `gilb-config`, `gilb-events`.
- Схема БД и миграции в `crates/gilb-db/migrations/`.
- Защита от password-полей, password masking, AX worker и budget
  логика — всё это инкапсулировано внутри `gilb-a11y`.
- `RecordingSettings::from_env` (`CAPTURE_EVENTS`, `CAPTURE_MOUSE_MOVE`,
  …) — helper тоже стартует с этими env vars из `gilb-config`.

**Lifecycle.** Helper стартует через `launchd` user agent при логине
пользователя; останавливается при logout. UI (gilb.app) запускается
поверх — открыт / закрыт независимо от helper'а. При первом запуске
gilb.app делает: проверить `SMAppService.status` → если
`NotRegistered` → `SMAppService.register()` → дальше polling
`AXIsProcessTrustedWithOptions` + deep-link в System Settings (как в
текущем `open_privacy_pane`, см. `apps/gilb-app-tauri/src-tauri/src/
lib.rs`).

## 3. IPC choices — XPC vs interprocess + msgpack

Карточка [GILB-4] предлагает XPC на macOS и Unix-socket на остальных
платформах. Разберём оба варианта против требований gilb:

**Требования:**
- Tauri-команды UI → helper: ≤ 100 запросов/мин, JSON-shape payloads.
- Capture stream НЕ идёт через IPC — capture-данные пишутся helper'ом
  прямо в SQLite. Tauri читает их через WAL.
- Bidirectional события: helper push'ит permission/health/session
  state в UI через subscription канал.
- Authentication: UI должна доверять только своему helper'у; helper
  должен доверять только своему UI.

**Вариант A — XPC через `objc2` напрямую.**

XPC — это Apple's IPC framework поверх Mach ports; нативная
аутентификация через audit tokens; integrated launchd activation
(сервис стартует по запросу через `SMAppService` mach service name).

Pros: zero-copy для больших dictionaries, нативная аутентификация
через `xpc_connection_get_audit_token`, idiomatic для SMAppService.

Cons: macOS-only — Windows придётся писать с нуля. XPC C API не
дружит с async Rust: dispatch_async-based, нужна обёртка через
`dispatch2` (v0.3.1, 2026-02). Зрелого Rust-крейта **нет**:
`xpc-connection` 0.2.3 опубликован в **2020-03** (6 лет, stale).
Realistic path — собственная FFI обёртка через `objc2` (0.6.4,
2026-02) + `dispatch2`, ≈ 400–600 строк unsafe Rust. На GILB-1 уже
зафиксировано "200–300 строк Swift через swift-bridge как
прагматичный путь без macOS-нативщика".

**Вариант B — `interprocess` + msgpack frames.**

`interprocess` 2.4.2 (2026-04) — кроссплатформенная Rust-библиотека:
Unix domain sockets на Linux/macOS, Named Pipes на Windows, единый
async API на tokio. `rmp-serde` 1.3.1 (2025-12) — `serde`-based msgpack
с поддержкой `#[derive]`. Frame layout: 4-byte big-endian length +
msgpack body.

Pros: один код на трёх ОС; интеграция с tokio out-of-the-box; знакомая
сериализация (как уже использует `apps/gilb-mcp` для MCP wire format);
маленький unsafe surface.

Cons: нет нативной аутентификации — нужно проверять `SO_PEERCRED`
(Linux) / `LOCAL_PEERPID` + `LOCAL_PEEREUID` (macOS) / pipe sid
(Windows) вручную. Нет launchd on-demand activation — helper всегда
работает в фоне (что для gilb всё равно требование).

**Выбор: вариант B.** Обоснование:

1. Кроссплатформенность — Windows phase 6 наступает скоро, переписывать
   IPC layer нерационально.
2. Нет stale-dependency риска: `interprocess` активно поддерживается,
   `xpc-connection` мёртв уже 6 лет.
3. Authentication достаточна на уровне Unix permissions: socket в
   `~/Library/Application Support/gilb/helper.sock` с mode 0600
   доступен только текущему UID. На Windows Named Pipe ACL — то же
   самое.
4. msgpack уже знаком команде (`rmcp` поверх него для MCP).

**Когда вернуться к XPC.** Если на phase 8+ выяснится, что нужно
запускать helper on-demand (например, для МСС server-mode без
постоянного процесса), XPC + Mach service name через SMAppService
mach service даст это бесплатно. Сейчас helper и так "always
running while logged in".

## 4. SMAppService integration path

`SMAppService` — это Apple's современный API для регистрации launch
agent'ов / daemon'ов / login items, доступный с **macOS 13.0
(Ventura, Oct 2022)**. `SMJobBless` (старый путь) deprecated и
отвалится в ближайших версиях.

Минимальный target gilb — `13.0` (`apps/gilb-app-tauri/src-tauri/
tauri.conf.json` → `minimumSystemVersion`), так что SMAppService
доступен везде.

**Rust path:**

Крейт **`objc2-service-management` 0.3.2 (2025-10)** — это
auto-generated binding на ServiceManagement framework от той же
команды, что делает `objc2`. Вызовы:

- `SMAppService::agent(plistName:)` создаёт agent service
- `SMAppService::register()` → `Result<(), NSError>`
- `SMAppService::status()` → `SMAppServiceStatus` (`NotRegistered` /
  `Enabled` / `RequiresApproval` / `NotFound`)
- `SMAppService::unregister()` для тестов и uninstall

Это **избавляет от swift-bridge fallback'а**, упомянутого в [GILB-4]
("свой Swift-бридж через `swift-bridge`, или `objc2` прямо в Rust").
Вся интеграция в pure Rust, ≈ 50–100 строк.

**LaunchAgent plist.** Helper должен жить как user agent (не daemon),
чтобы:

- Работать без `sudo`.
- Получать доступ к user-specific SQLite в `~/.gilb/db.sqlite`.
- Запускаться при login и останавливаться при logout.

Plist `ai.gilb.helper.plist` — внутри
`Gilb.app/Contents/Library/LaunchAgents/`:

```xml
<dict>
  <key>Label</key>                 <string>ai.gilb.helper</string>
  <key>BundleProgram</key>         <string>Contents/Resources/helpers/gilb-helper</string>
  <key>RunAtLoad</key>             <true/>
  <key>KeepAlive</key>             <true/>
  <key>StandardErrorPath</key>     <string>/tmp/gilb-helper.err</string>
  <key>MachServices</key>          <dict>...</dict>  <!-- если позже добавим XPC -->
</dict>
```

`MachServices` нужен только если переходим на XPC (см. § 3). Пока
есть `interprocess`-сокет, MachServices в plist не пишем.

**Signing constraints.** Это самая хрупкая часть. Helper должен быть:

- Подписан тем же Team ID `83856566PM`, что main.
- Иметь hardened runtime (`--options runtime`).
- Иметь свой собственный entitlements.plist (минимум:
  `com.apple.security.app-sandbox=false`, плюс то, что нужно для AX —
  фактически ничего из entitlements, AX — TCC-permission).
- Notarized через тот же notary submission, что main app.

`tauri-bundler` сегодня умеет один main binary. Чтобы добавить
embedded helper:

1. В `Cargo.toml` `apps/gilb-app-tauri/src-tauri` (или его child)
   добавить второй `[[bin]]` с именем `gilb-helper`, или собирать
   helper из workspace bin'аря `crates/gilb-helper`.
2. В `tauri.conf.json` под `bundle.resources` указать путь к
   собранному helper-бинарю и LaunchAgent plist'у.
3. После `tauri build` post-process script (Cargo build script или
   отдельный shell в CI) делает `codesign` отдельно для helper'а с
   правильным DR и его entitlements.

Альтернатива: собирать helper отдельным workspace member и
подкладывать его в `Contents/Resources/helpers/` через
`beforeBundleCommand` Tauri-hook. Это чище в смысле separation,
но требует ручной интеграции с notary.

**Risk: SMAppService user approval dialog.** При первом
`SMAppService.register()` macOS показывает алерт "Gilb wants to add
items that can run in background" со ссылкой в "Login Items" pane
System Settings. Если пользователь disable'ит — helper не запустится,
а UI должна понять это и показать onboarding. Polling
`SMAppService.status() == .RequiresApproval` + deep-link в
`x-apple.systempreferences:com.apple.LoginItems-Settings.extension`.
Это второй onboarding-шаг после AX permission.

## 5. SQLite single-writer / multi-reader

Текущее устройство: `gilb-db::open_db` (см. `crates/gilb-db/src/db.rs`
по референсу из `CLAUDE.md`) открывает один SQLite pool с WAL,
`busy_timeout=5s`, `wal_autocheckpoint=4000`. Writer и reader-запросы
обслуживаются из одного pool'а внутри Tauri shell'а.

**Что меняется с helper'ом:**

- **Writer pool — только в helper.** Открыт с
  `OpenFlags::READ_WRITE | OpenFlags::CREATE` (или sqlx
  `SqliteConnectOptions::new().create_if_missing(true)`). Все
  `INSERT` / `UPDATE` / миграции прогоняются helper'ом. Это естественно,
  потому что capture-стрим уже в helper'е, и держать write-canal в
  Tauri бессмысленно.
- **Reader pool — в Tauri.** Открыт с `OpenFlags::READ_ONLY`. WAL уже
  включён writer'ом, reader просто видит committed transactions.
  Reader НЕ запускает миграции — это исключительно ответственность
  writer'а (helper стартует первым).
- **MCP — внутри helper.** MCP server переиспользует writer-pool (для
  consistency reads из той же транзакции, что и writer state).
  Альтернатива — отдельный read-only pool в том же процессе, но это
  лишняя сложность для read latency.

**Race на старте.** Reader (Tauri) может запуститься раньше, чем
writer (helper) — например, пользователь открывает gilb.app до того,
как launchd прокинул helper. Решение:

- Tauri опрашивает `SMAppService.status()` и `helper.sock` существует
  → если нет, показывает "Helper not ready" splash до 5 секунд.
- При первом запуске (DB не существует) — reader падает с
  `SQLITE_CANTOPEN`. Tauri должен это распознать и подождать; не
  создавать DB сам — это поломает миграционную инвариантность
  "writer создаёт схему".

**WAL hygiene.** Помимо `wal_autocheckpoint=4000`, при остановке
helper'а (`stop_capture` или SIGTERM) делаем
`PRAGMA wal_checkpoint(TRUNCATE)`. Это уменьшает `-wal` файл, чтобы
после crash recovery занимало меньше времени.

**Lock contention.** SQLite WAL разрешает один writer и много
readers, **но** при `BEGIN IMMEDIATE` writer держит write-lock на
файл. С одним writer'ом (helper) и многими readers (Tauri,
MCP-clients, debug инструменты типа `sqlite3 CLI` пользователя)
contention минимален. `busy_timeout=5s` достаточен.

**Reader staleness.** Reader видит committed WAL pages — задержка
зависит от частоты writer'ом коммитов. Сегодня
`ACTION_CHANNEL_CAPACITY = 4096`, writer вставляет по одному
(Phase 3 заменит на batched ≤500 ops/TX). После batching reader
будет видеть данные с задержкой ≤ 1 batch interval (~100ms),
что приемлемо для UI ("recent actions feed").

**Crate choice.** Текущий workspace на `sqlx` 0.9.0 (см. workspace
`Cargo.toml`). `sqlx` поддерживает open mode через
`SqliteConnectOptions::read_only(true)`. Альтернатива `rusqlite`
0.39.0 (2026-03) — sync-API, что для writer'а внутри tokio task
требует `spawn_blocking`. Решение: **остаёмся на `sqlx`** во всех
крейтах. Writer-pool в helper'е, reader-pool в Tauri через
`SqlitePool::connect_with(... .read_only(true))`.

## 6. MCP server relocation

Текущее устройство: `apps/gilb-mcp` — отдельный crate с своим
`main.rs`, который открывает SQLite read-only и предоставляет MCP
tools через `rmcp` (см. `apps/gilb-mcp/src/service.rs`,
`queries.rs`). В Claude Desktop config указан абсолютный путь к
`gilb-mcp` бинарю.

Проблемы такого устройства:

- Дублирование. `gilb-db` open + миграция-проверка + connection
  setup живут в двух местах.
- Permission split. `gilb-mcp` НЕ держит AX-permission, ему оно и
  не нужно (только чтение БД); но если когда-нибудь захотим
  exposed-через-MCP действия типа `take_screenshot` или
  `start_session_now`, придётся писать кросс-процесс RPC от MCP
  к helper'у.
- Lifecycle. MCP-сервер живёт пока живёт Claude Desktop; если Claude
  Desktop спавнит много инстансов — лишние SQLite-handles.

После relocation в helper:

- `crates/gilb-helper` имеет два режима запуска: `gilb-helper`
  (default, daemon mode — слушает `helper.sock` и стартует capture);
  `gilb-helper --mcp-stdio` (одноразовый — подключается к уже
  работающему daemon'у через `helper.sock` и проксирует MCP трафик
  через stdio).
- `apps/gilb-mcp` **удаляется**. Его источники (`queries.rs`,
  `range.rs`, `service.rs`) переезжают в `crates/gilb-helper/src/mcp/`.
- Claude Desktop config:

  ```json
  {
    "mcpServers": {
      "gilb": {
        "command": "/Applications/Gilb.app/Contents/Resources/helpers/gilb-helper",
        "args": ["--mcp-stdio"]
      }
    }
  }
  ```

- Команда `mcp_connect` в `apps/gilb-app-tauri/src-tauri/src/commands/
  mcp_connect.rs` обновляется на новый путь.

**Single-binary трюк.** `gilb-helper --mcp-stdio` запускается как
short-lived процесс под Claude Desktop'ом. Он НЕ дублирует daemon
state — он просто IPC-клиент к долгоживущему `gilb-helper`-daemon'у
(который стартует через SMAppService на login). Если daemon не
запущен (например, на dev машине), `--mcp-stdio` пытается стартануть
его через `SMAppService.register()` (или fail с понятным сообщением,
если нет .app бандла).

**Read-only из MCP.** MCP tools (`get_recent_actions`, `search_text`,
`get_patterns`) должны быть pure read. Если когда-нибудь добавим
"write" MCP-tools (например, `start_capture_session`), они идут
через тот же IPC канал, который использует Tauri — общий
`helper_client::send(Command::*)`. Это автоматически даёт consistency.

## 7. Revised phase plan

Текущий `tauri-plan.md` живёт в pre-helper модели (Engine внутри
Tauri). Helper-разделение влияет на Phase 3+. Ниже — что меняется по
phase'ам; полную rewrite `tauri-plan.md` оставляем на отдельную
карточку (это не входит в scope [GILB-17]).

### Сабкарточки имплементации (уже созданы)

- **[GILB-18]** — Helper crate skeleton + Unix-socket IPC. Создаём
  `crates/gilb-helper` с минимальным `main()` (logs alive, слушает
  socket). `gilb-engine` переезжает с Tauri на helper. Линукс- и
  macOS-first; Windows стаб.
- **[GILB-19]** — macOS SMAppService integration. Bundle layout +
  LaunchAgent plist + `objc2-service-management` calls + onboarding
  flow с deep-link и pollingом `AXIsProcessTrustedWithOptions`.
- **[GILB-20]** — Helper owns SQLite writer. Tauri переходит на
  `SqliteConnectOptions::read_only(true)`. Все `INSERT`-функции в
  `gilb-db` живут в helper'е.
- **[GILB-21]** — MCP server relocation. `apps/gilb-mcp` → удаляем,
  код переезжает в `crates/gilb-helper/src/mcp/`. Claude Desktop
  config обновляется.

### Влияние на существующие Tauri-plan Phase'ы

- **Phase 0 (Foundation)** — выполнен. Не трогаем.
- **Phase 1 (Event capture macOS)** — выполнен. Не трогаем.
- **Phase 2 (Tree capture + budget)** — выполнен. Не трогаем.
- **Phase 3 (Robustness)** — должен быть **переоформлен**:
  write_queue, ImmediateTx, recovery CLI — всё это теперь в helper'е.
  Sleep monitor и permission revoke detection остаются в `gilb-a11y`
  (он уже cfg-разделённая crate). Это не запрещает Phase 3 закончить
  ДО helper-split'а — просто write_queue потом мигрирует вместе с
  `gilb-engine`.
- **Phase 4 (UI polish, tray, ServerCore split)** — переоформляется:
  tray и pause-кнопка переезжают в Tauri main, но `pause_capture`
  идёт IPC-запросом в helper. ServerCore vs CaptureSession разделение
  логически становится "helper-daemon (всегда живёт) vs CaptureSession
  (стартует/стопится по команде)" — именно то, что предложено в
  [GILB-4].
- **Phase 5 (FTS5 search)** — read-only в Tauri, MCP пишет через
  helper. FTS триггеры остаются writer-side.
- **Phase 6 (Windows backend)** — взаимодополняет helper: Windows
  Service / scheduled task под `interprocess` Named Pipe.
- **Phase 7 (Layer 1 gate)** — добавляется acceptance criterion
  "TCC permission survives auto-update of main app while helper
  unchanged".

### Порядок имплементации

[GILB-18] (skeleton) → [GILB-20] (writer move) → [GILB-19] (SMAppService)
→ [GILB-21] (MCP relocation). Логика порядка: skeleton нужен всем;
writer move ортогонален SMAppService; MCP relocation — финальный
шаг (зависит от того, что daemon уже стабильно работает).

## 8. Yes/No recommendation + risk register

### Решение: **YES**, адопция helper-daemon архитектуры.

**Главные аргументы за:**

1. **MCP continuity.** Это самый сильный аргумент. Сегодня
   `apps/gilb-mcp` имеет два режима: либо open-on-demand из Claude
   Desktop (значит SQLite-handle создаётся каждый раз), либо
   long-running с собственным lifecycle, что дублирует helper.
   Helper unifies — один процесс владеет SQLite, capture, MCP.
2. **Hot writes не через JS-bridge.** Уже сегодня capture работает
   в Rust-таске внутри Tauri-shell'а (не через JS), так что эта
   проблема **частично решена**. Но Tauri main процесс перезапускается
   при auto-update — capture теряется. Helper это снимает.
3. **TCC permission stability** — реально становится load-bearing
   только если будет ребрендинг или смена Team ID. На стабильной
   подписи main + helper приносит маржинальную выгоду; **главный
   выигрыш не здесь**.
4. **Кросс-платформенность.** Архитектура изоморфна на Windows
   (Service) и Linux (systemd user unit), хотя Linux вне scope.
   `interprocess` даёт единый IPC.
5. **Future-proof для Layer 2+.** PrefixSpan miner и cold compactor
   из [GILB-4] логически в helper'е, чтобы UI был free
   "вычислительной массы". Это разделение лучше внести сейчас.

**Главные аргументы против (и почему они проиграли):**

1. Один процесс проще дебажить → проиграл, потому что Layer 1 уже
   достаточно сложный, чтобы IPC-разделение помогло, а не
   помешало (clearer error boundaries).
2. Bundling helper'а удваивает code-signing complexity → проиграл,
   потому что один раз настроить CI lane проще, чем годами
   объяснять пользователям, почему permission слетел.
3. Helper-daemon overkill для MVP → проиграл, потому что
   relocation позже (когда Layer 2 уже в Tauri shell) дороже.

### Risk register

| # | Риск | Likelihood | Impact | Mitigation |
|---|------|-----------|--------|------------|
| R1 | `tauri-bundler` не умеет встроенный helper out of the box; нужен post-bundle codesign | high | medium | beforeBundleCommand + отдельный codesign step в CI; есть прецеденты в Tauri community |
| R2 | SMAppService.register() показывает "Login Items" alert — пользователь может disable | medium | high | onboarding-step с polling `.status()` и deep-link в Login Items pane |
| R3 | `objc2-service-management` 0.3.2 (2025-10) borderline по recency (~7 мес) — может потребоваться форк | low | low | API простой (3-4 вызова); fallback — `objc2` напрямую к ServiceManagement framework, ≈ 100 строк unsafe |
| R4 | Notarization двух бинарей в одном бандле — нестандартный flow | medium | medium | apple notarytool принимает .app целиком, helper нотаризуется как nested binary автоматически |
| R5 | Reader (Tauri) стартует раньше writer (helper) и падает на migrations | medium | low | helper-writer стартует от launchd на login; Tauri показывает splash до 5s; никогда не запускает миграции из reader pool'а |
| R6 | TCC reset / quarantine reset убивает grant — helper не спасает | low | medium | документация в help.md; не наша зона ответственности; minimum gilb может предложить — pre-flight check в onboarding |
| R7 | `xpc-connection` крейт мёртв (2020) — если позже захотим XPC, придётся писать через `objc2`+`dispatch2` | medium | low | сейчас не выбираем XPC; если придёт — есть проверенный путь через `objc2`+`dispatch2` v0.3.1 |
| R8 | `launchd` крейт мёртв (2023) — нужен ручной plist | high | low | plist генерируем bash-скриптом в CI или храним как ресурс; крейт не нужен |
| R9 | Single-binary `gilb-helper --mcp-stdio` стартует daemon если того нет — на dev-машине без .app бандла fail-mode неочевиден | low | low | явное сообщение "daemon not running, install Gilb.app first"; для разработки — `cargo run --bin gilb-helper` руками |
| R10 | Tauri auto-updater пересоздаёт `.app` целиком — helper-плист в `Contents/Library/LaunchAgents/` тоже пересоздаётся, и launchd может зашатать | low | medium | использовать `SMAppService.register()` каждый запуск (идемпотентно); проверять `.status()` |

### Validation rubric для следующих карточек

Для каждой из [GILB-18]..[GILB-21] PR должен показать:

- DoD-критерии исходной карточки выполнены.
- Сохраняется backward-compat read-path (т.е. старый `apps/gilb-mcp`
  бинарь не ломается до полного relocation в [GILB-21]).
- `cargo test --workspace` зелёный.
- `cargo clippy --workspace --all-targets` без новых warnings.

После завершения [GILB-21]:

- Acceptance: install signed Gilb.app → grant AX → `tauri build` v2
  → install → AX permission survives (TCC.db показывает запись для
  `ai.gilb.helper` неизменной).
- Acceptance: Claude Desktop connects к helper'у; `get_recent_actions`
  возвращает данные после закрытия gilb.app главного окна.

## 9. Crate inventory (валидировано на 2026-05-23)

Сводная таблица крейтов, которые цитируются выше. Все цифры из
`crates.io/api/v1/crates/...`.

| Крейт | Версия | Дата | Назначение | Состояние |
|---|---|---|---|---|
| `interprocess` | 2.4.2 | 2026-04 | UDS / Named Pipes IPC | fresh |
| `rmp-serde` | 1.3.1 | 2025-12 | msgpack frames (serde) | fresh |
| `rmp` | 0.8.15 | 2025-12 | msgpack primitives | fresh |
| `objc2` | 0.6.4 | 2026-02 | Obj-C runtime bindings | fresh |
| `objc2-service-management` | 0.3.2 | 2025-10 | SMAppService.register/status/unregister | fresh |
| `objc2-foundation` | 0.3.2 | 2025-10 | NSString / NSError | fresh |
| `dispatch2` | 0.3.1 | 2026-02 | libdispatch (для XPC fallback'а) | fresh |
| `swift-bridge` | 0.1.59 | 2026-01 | fallback на Swift shim | fresh (не нужен в выбранном пути) |
| `rmcp` | 1.7.0 | 2026-05 | MCP server SDK | fresh (уже в workspace) |
| `rusqlite` | 0.39.0 | 2026-03 | sync SQLite (если откажемся от sqlx) | fresh |
| `sqlx` | 0.9.0 | 2026-05 | async SQLite (текущий выбор) | fresh (уже в workspace) |
| `prior-art` | 0.15.1 | 2026-05 | Apple framework wrappers | fresh (уже в workspace) |
| `accessibility-sys` | 0.2.0 | 2025-03 | AX C API bindings | borderline (~14 мес, активно используется) |
| `core-foundation` | 0.10.1 | 2025-05 | CF types | borderline (~12 мес) |
| `core-graphics` | 0.25.0 | 2025-05 | CG types | borderline (~12 мес) |
| `xpc-connection` | 0.2.3 | 2020-03 | XPC client | **stale 6 лет — не использовать** |
| `launchd` | 0.3.0 | 2023-08 | launchd plist helpers | **stale 2.5 года — не использовать** |
| `parity-tokio-ipc` | 0.9.0 | 2021-07 | альтернатива interprocess | **stale — interprocess лучше** |

**Версии не пинятся** — это задача карточек имплементации. Здесь
только подтверждаем, что нужные крейты существуют и активно
поддерживаются.
