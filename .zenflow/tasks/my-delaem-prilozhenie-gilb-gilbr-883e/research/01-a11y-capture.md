# screenpipe-a11y: захват действий пользователя

Crate: `/Users/leonid/src/gilb/screenpipe/crates/screenpipe-a11y/`

Это **самый важный** для Gilb crate — он отвечает за всё, что мы планируем
собирать: input events + accessibility tree + window/app focus.

## 1. Зависимости (Cargo.toml:48-86)

Нативные платформо-специфичные API, **без обёрток типа `rdev`**:

| Платформа | Что используют | Покрытие |
|-----------|----------------|----------|
| macOS | `cidre@0.13` (ax, cg, blocks, ns, dispatch) | CGEventTap, AXUIElement, NSWorkspace, NSPasteboard |
| Windows | `windows@0.58` (Win32_UI_Accessibility, Com) | UI Automation COM + SetWindowsHookEx |
| Linux | `zbus@5` + `evdev@0.13` | AT-SPI2 D-Bus + raw `/dev/input/event*` |
| Все | `arboard@3` | Clipboard (нативные APIs) |

## 2. Что собирается

**Input events** (`src/events.rs:47-124`, enum `EventData`):

| Event | Поля |
|-------|------|
| Click | x, y, button (0/1/2), click_count, modifiers, ElementContext |
| Move | x, y (после threshold 5px) |
| Scroll | x, y, delta_x, delta_y |
| Key | key_code, modifiers — **без символов** (privacy by default) |
| Text | content, char_count — агрегация с timeout 300 мс |
| AppSwitch | name, pid |
| WindowFocus | app, title |
| Clipboard | operation ('c'/'x'/'v'/'p'), content (optional) |

**ElementContext** для кликов (`events.rs:126-151`) — role / name / value /
description / automation_id / bounds. То самое, что нужно для therblig: "user
clicked a Button labeled 'Send' at (x,y)".

**AccessibilityNode** для полных tree snapshots (`events.rs:166-234`):
control_type, name, automation_id, class_name, value, bounds, is_enabled,
is_focused, is_selected, is_expanded, is_password, иерархия `children`.

**WindowTreeSnapshot** (`events.rs:437-461`): timestamp, app_name,
window_title, pid, root node, element_count, **tree_hash** для dedup.

**LineSpan** (`tree/mod.rs:62-67`): per-line geometry для multi-line text
(можно показывать точные bounding boxes слов).

## 3. Механизм сбора: hybrid

### macOS (`src/platform/macos.rs`)

Три потока, минимально связанные:

1. **CGEventTap thread** (input events) — `EventTapLocation::Session`,
   `LISTEN_ONLY`, callback `tap_callback` (`macos.rs:677+`).
   - **Worker threads для context capture** (`macos.rs:568-595`): tap
     callback кладёт запрос в bounded channel (size=4); отдельный worker
     дёргает `AXUIElementCopyElementAtPosition`. Никакой блокировки tap'а.
   - **Text buffer** с timeout 300 мс (`macos.rs:502-541`) агрегирует символы,
     обрабатывает backspace, flush при изменении или таймауте.

2. **AX Observer thread** (focus tracking) — слушает NSWorkspace notifications
   (`did_activate_app`, `active_space_did_change`, `did_wake`); динамически
   переподписывает observer на новый фокусный pid (`macos.rs:1100-1143`):
   `app_activated`, `focused_window_changed`, `focused_ui_element_changed`.

3. **Clipboard poller** — 750 мс (`macos.rs:383`), читает `NSPasteboard.changeCount()`
   (cheap); при изменении — worker thread читает контент **на main queue**
   через `dispatch::Queue::main().sync_once()` (`macos.rs:1434-1446`).

**Critical**: `static AX_QUERY_LOCK: parking_lot::Mutex<()>` (`macos.rs:28`) —
AX queries нельзя гонять параллельно (могут портить AppKit caches).
Context capture использует **try-lock**: если занято — пропускает (не блокирует
event tap).

### Windows (`src/platform/windows_uia.rs`)

- `SetWindowsHookEx` (WH_MOUSE_LL + WH_KEYBOARD_LL) на отдельном thread.
- UIA `IUIAutomation` COM на apartment-threaded worker'е.
- **CacheRequest batching** (`windows_uia.rs:92-117`): одна COM call —
  все свойства subtree сразу (огромный выигрыш по latency).
- Control View фильтрует ~50% layout-only nodes.
- TreeWalker fallback для Chromium/Electron (`windows_uia.rs:175-186`).
- COM-based `IUIAutomationFocusChangedEventHandler`.

### Linux (`src/platform/linux.rs`)

- `evdev` поток читает `/dev/input/event*` (требует группу `input`).
- `zbus::blocking` подписывается на AT-SPI2 focus signals.
- X11: `xdotool getactivewindow`; Wayland: D-Bus screencast portal.

## 4. Adaptive FPS (activity_feed.rs:92-134)

Без этого long-term запись либо жжёт CPU, либо теряет события.

| Состояние | Интервал | FPS |
|-----------|----------|-----|
| Keyboard burst (3+ key / 500 мс) | 200 мс | 5 |
| Active typing (kb idle < 300 мс) | 150 мс | ~7 |
| General activity | 200 мс | 5 |
| Cooling | 500 мс | 2 |
| Idle | 1000 мс | 1 |
| Deep idle | 2000 мс | 0.5 |

`tree_debounce_ms: 300` (config.rs:54) — после focus change ждём 300 мс
прежде чем обходить дерево (UI часто меняется в первые мс).

## 5. Throttling per app (budget.rs:14-194)

Самая интересная оптимизация. Бюджет **на каждое приложение**:

```text
if avg_walk_duration >= 250ms OR truncated_count >= 3
    → Critical: 60s interval, 500 nodes max
else if avg_walk_duration >= 150ms
    → Heavy: 5s interval, 1000 nodes
else if avg_walk_duration >= 50ms
    → Moderate: 2s interval, 2000 nodes
else
    → Light: 200ms interval, 5000 nodes
```

Discord / Slack / огромные Electron-приложения автоматически уходят в Critical
вместо того чтобы убить машину. **Это must-have для Gilb.**

## 6. Deduplication деревьев (tree/cache.rs)

`TreeCache` + **SimHash** с Hamming distance:

```rust
should_store(snapshot) =
    hamming_distance(prev_hash, new_hash) > 10 bits
    OR ttl_expired (60s)
```

Скролл меняет ~10–20% контента → ~5–10 bit diff → не сохраняем дубль. Снижает
write throughput в 10× для типичных сценариев (чтение / скролл).

`tree/enhanced_mode_cache.rs` — per-(app, window) cache с timestamp +
content hash для быстрого short-circuit.

## 7. macOS-специфичные трюки в дереве (tree/macos.rs)

- `AXDocument` extraction: file URLs для TextEdit/Xcode, http(s) для браузеров.
- **Electron app state file resolution**: например, Obsidian — читает
  `obsidian.json` + `workspace.json`, чтобы знать какой vault и какой
  активный файл (т.к. через AX непонятно).
- Per-line bounds через `AXBoundsForRange`.
- URL извлекается тройным fallback: AXDocument → AppleScript (Arc) → shallow
  walk AXTextField (адресная строка).

## 8. Lock-free hot path

`current_app` / `current_window` — `arc_swap::ArcSwap<Option<String>>`
(`macos.rs:237-238`). Event tap callback читает state **без mutex'а**, observer
thread обновляет через CAS. Zero contention на самом горячем пути.

## 9. Permission model и graceful degradation

| Permission | macOS | Что отвалится без него |
|------------|-------|------------------------|
| Accessibility (AXUIElement) | hard required | всё дерево |
| Input Monitoring (CGEventTap) | optional | input events; clipboard polling + app switches остаются |
| Screen Recording | not needed для a11y | (нужен для screen capture) |

Идея: **не падать**, а отдавать reduced feature set с предупреждением.

## 10. Privacy / PII

- `is_password_field()` (`config.rs:317-352`) — role `AXSecureTextField`,
  `PasswordBox` + heuristic по name ("password", "pin", "secret", "api key",
  "access token", "passwort", ...).
- `skip_secure_input: true` на macOS (NSSecureTextInput).
- `excluded_apps`: 1Password / Bitwarden / KeePassXC / LastPass / Dashlane /
  Keychain Access / Credential Manager (`config.rs:194-212`).
- `excluded_window_patterns: Vec<Regex>`, `ignored_windows`, `included_windows`
  (allowlist), `apply_pii_removal: true` (regex по email/ssn/cc).
- **Incognito detection**: macOS AppleScript, Windows window-title patterns,
  Linux GNOME env.

## 11. Что забрать в Gilb

1. **Hybrid event model** — event tap + AX observer + clipboard poller, со
   строгим разделением "капчер" / "контекст" через bounded channels.
2. **Adaptive FPS + per-app budget** — без этого долгая запись невозможна.
3. **SimHash dedup** для деревьев.
4. **Lock-free ArcSwap для current_app/window**.
5. **Try-lock pattern** для AX queries (никогда не блокируем hot path).
6. **Privacy by default**: keycodes без символов, password-field detection,
   excluded apps list, incognito detection.
7. **`ElementContext` для click'ов** — это уже фактически atomic therblig.
8. **WindowTreeSnapshot + tree_hash** — основа для diff-анализа состояний UI.

## 12. Что упростить для Gilb v0

- Gilb должен работать на **macOS И Windows** в дальнейшем; Linux вне scope.
- **MVP — macOS-first** (cidre + CGEventTap + AXUIElement), но архитектуру
  сразу проектируем с per-platform trait'ами (как `platform/macos.rs` vs
  `platform/windows_uia.rs` у screenpipe). Windows ветка добавляется сразу
  после proof-of-concept на macOS, не "когда-нибудь потом".
- На Windows будем использовать ту же модель: `SetWindowsHookEx` (mouse+kb
  low-level hooks) + UI Automation COM через crate `windows@0.58` с
  CacheRequest batching.
- Дерево можно начинать с **focused window only** (без full system walk) на
  обеих платформах.
- `tree/enhanced_mode_cache.rs` и Electron app-state resolution — overkill
  для MVP.
- Image PII (rfdetr-mlx) точно не нужен в v0.
