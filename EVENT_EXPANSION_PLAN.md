# Gilb Event Expansion Plan
## Добавление новых типов событий и контекста

**Дата:** 2025-06-19
**Статус:** Draft
**Цель:** Собирать больше контекста о действиях пользователя, как в Codex Record & Replay

---

## Current State Analysis

### ✅ Что УЖЕ реализовано в gilb

#### Focus (частично)
```rust
pub struct FocusSnapshot {
    pub app: AppInfo,              // ✅ Приложение
    pub focused_role: Option<String>,  // ✅ Роль элемента (TextField, Button, ...)
    pub focused_secure: bool,      // ✅ Secure field flag
}
```

**Используется для:**
- Masking текста в secure полях
- Password флаг на actions

**❌ Чего не хватает в фокусе:**
- Нет имени сфокусированного элемента (`focused_name`)
- Нет identifier сфокусированного элемента
- Нет позиции сфокусированного элемента
- Нет отдельного события при смене элемента (только окна/приложения)
- FocusChange событие несёт только app info, не element info

#### AppInfo (полностью)
```rust
pub struct AppInfo {
    pub bundle_id: Option<String>,   // ✅
    pub name: Option<String>,         // ✅
    pub pid: Option<i32>,            // ✅
    pub window_title: Option<String>, // ✅
    pub browser_url: Option<String>,  // ✅ (Chrome, Safari, Firefox, Edge, ...)
}
```

#### ElementContext (для кликов)
```rust
pub struct ElementContext {
    pub role: Option<String>,       // ✅ AX Role
    pub name: Option<String>,       // ✅ AX Title
    pub value: Option<String>,      // ✅ AX Value
    pub help: Option<String>,       // ✅ AX Help
    pub identifier: Option<String>,  // ✅ AX Identifier
    pub frame: Option<Frame>,       // ✅ {x, y, w, h}
}
```

### ❌ Что НЕТ в gilb (по сравнению с Codex)

| Категория | Codex имеет | gilb статус | Priority |
|-----------|-------------|--------------|----------|
| **Selection** | selectedText, selectedRange, selectedRows/Columns | ❌ Нет | P0 |
| **Click detail** | clickCount, modifiers | ❌ Нет | P1 |
| **Drag & Drop** | origin, destination, drag events | ❌ Нет | P2 |
| **Focus element** | Полный element context для focus | ⚠️ Только role | P1 |
| **Table context** | selectedRows/Columns events | ❌ Нет | P2 |

---

## Implementation Plan

### Phase 1: Selection Tracking (P0) — Week 1

**Цель:** Знать что пользователь выделил и где курсор

#### 1.1 Расширить ElementContext

```rust
// crates/gilb-core/src/lib.rs

pub struct SelectionRange {
    pub start: usize,
    pub end: usize,
}

pub struct ElementContext {
    // Существующие поля
    pub role: Option<String>,
    pub name: Option<String>,
    pub value: Option<String>,
    pub help: Option<String>,
    pub identifier: Option<String>,
    pub frame: Option<Frame>,

    // НОВЫЕ поля
    pub selected_text: Option<String>,       // Выделенный текст
    pub selection_range: Option<SelectionRange>, // Диапазон {start, end}
    pub cursor_position: Option<usize>,     // Позиция курсора
}
```

#### 1.2 AX integration для selection

```rust
// crates/gilb-a11y/src/platform/macos/ax_worker.rs

fn element_at(
    system: AXUIElementRef,
    x: f64,
    y: f64,
    focus: &FocusState,
) -> Option<ElementContext> {
    // ... существующий код ...

    // НОВОЕ: Извлечь выделение
    let selected_text = read_string_attr(element, ax_attr_name("AXSelectedText"));
    let selection_range = read_range_attr(element, ax_attr_name("AXSelectedTextRange"));

    Some(ElementContext {
        role,
        name,
        value,
        help,
        identifier,
        frame,
        selected_text,           // NEW
        selection_range,          // NEW
        cursor_position: None,    // NEW (set via focused element)
    })
}

// НОВОЕ: Функция для чтения range
fn read_range_attr(element: AXUIElementRef, attr: CFString) -> Option<SelectionRange> {
    let mut value: CFTypeRef = ptr::null_mut();
    let res = unsafe { AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value) };
    if res != kAXErrorSuccess || value.is_null() {
        return None;
    }

    // AXSelectedTextRange возвращает CFDictionary с {loc, len}
    // или CFArray для множественных диапазонов
    unsafe {
        if CFGetTypeID(value) == CFDictionary::type_id() {
            let dict = CFDictionary::from_CFType(value as CFTypeRef);
            if let Some(loc) = dict.get(kCFNumberLongType) {
                if let Some(len) = dict.get(kCFNumberLongType) {
                    // TODO: Parse CFDictionary
                }
            }
        }
    }
    None  // Stub - нужно реализовать
}
```

#### 1.3 Миграция БД

```sql
-- crates/gilb-db/migrations/0006_selection_context.sql

ALTER TABLE actions ADD COLUMN selected_text TEXT;
ALTER TABLE actions ADD COLUMN selection_range TEXT;  -- JSON: {"start":0,"end":10}
ALTER TABLE actions ADD COLUMN cursor_position INTEGER;

CREATE INDEX idx_actions_selection
    ON actions(session_id, captured_at)
    WHERE selected_text IS NOT NULL;
```

#### 1.4 Файлы для изменения

- `crates/gilb-core/src/lib.rs` — Add SelectionRange, extend ElementContext
- `crates/gilb-a11y/src/platform/macos/ax_worker.rs` — Extract selection from AX
- `crates/gilb-db/src/actions.rs` — Handle new columns
- `crates/gilb-db/migrations/0006_selection_context.sql`

---

### Phase 2: Focus Element Tracking (P1) — Week 2

**Цель:** Полный контекст сфокусированного элемента, не только роль

#### 2.1 Расширить FocusSnapshot

```rust
// crates/gilb-a11y/src/focus.rs

pub struct FocusSnapshot {
    pub app: AppInfo,
    
    // Существующие
    pub focused_role: Option<String>,
    pub focused_secure: bool,
    
    // НОВЫЕ - полный контекст элемента
    pub focused_element: Option<ElementContext>,  // Полный element
}
```

#### 2.2 AX integration для focused element

```rust
// crates/gilb-a11y/src/platform/macos/focus.rs

/// Получить сфокусированный элемент через AX
fn get_focused_element(app_elem: AXUIElementRef) -> Option<ElementContext> {
    let focused_attr = CFString::new("AXFocusedUIElement");
    let mut value: CFTypeRef = ptr::null_mut();
    let res = unsafe {
        AXUIElementCopyAttributeValue(app_elem, focused_attr.as_concrete_TypeRef(), &mut value)
    };
    if res != kAXErrorSuccess || value.is_null() {
        return None;
    }

    let element = value as AXUIElementRef;
    // Извлечь все атрибуты элемента...
    Some(read_element_context(element))
}
```

#### 2.3 Использовать в normalizer

```rust
// crates/gilb-a11y/src/normalizer.rs

async fn emit_text(&self, flushed: FlushedText, snap: &FocusSnapshot) {
    let elem = snap.focused_element
        .clone()
        .unwrap_or_default();

    let action = Action {
        session_id: self.session_id,
        captured_at: Utc::now(),
        kind: ActionKind::Text,
        app: snap.app.clone(),
        element: elem,  // Теперь полный контекст, не только role
        text_content: Some(content),
        password_flag: masked,
        // ...
    };
    // ...
}
```

#### 2.4 Миграция БД (опционально)

```sql
-- crates/gilb-db/migrations/0007_focused_element.sql

-- Для событий где элемент был сфокусирован
ALTER TABLE actions ADD COLUMN was_focused INTEGER DEFAULT 0;

CREATE INDEX idx_actions_focused
    ON actions(session_id, captured_at)
    WHERE was_focused = 1;
```

---

### Phase 3: Click Detail (P1) — Week 2-3

**Цель:** Отличать double-click, знать модификаторы

#### 3.1 Расширить Action

```rust
// crates/gilb-core/src/lib.rs

pub enum Modifier {
    Command,
    Control,
    Option,
    Shift,
    Fn,
    CapsLock,
}

pub struct Action {
    // Существующие поля
    pub session_id: SessionId,
    pub captured_at: DateTime<Utc>,
    pub kind: ActionKind,
    pub app: AppInfo,
    pub element: ElementContext,
    pub text_content: Option<String>,
    pub password_flag: bool,
    pub tree_snapshot_id: Option<TreeSnapshotId>,
    pub extra_json: Option<serde_json::Value>,

    // НОВЫЕ - click detail
    pub click_count: Option<u8>,           // 1=single, 2=double, 3=triple
    pub modifiers: Option<Vec<Modifier>>,   // Активные модификаторы
}
```

#### 3.2 EventTap для click_count и modifiers

```rust
// crates/gilb-a11y/src/platform/macos/event_tap.rs

struct ClickState {
    last_click_time: Instant,
    last_click_button: Option<MouseButton>,
    click_count: u8,
}

impl ClickState {
    fn register_click(&mut self, button: MouseButton) -> u8 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_click_time);

        // Double-click если < 500ms и та же кнопка
        if elapsed < Duration::from_millis(500) && self.last_click_button == Some(button) {
            self.click_count = (self.click_count + 1).min(3);
        } else {
            self.click_count = 1;
        }

        self.last_click_time = now;
        self.last_click_button = Some(button);
        self.click_count
    }
}

fn decode_event(etype: CGEventType, event: &CGEvent) -> Option<RawEvent> {
    match etype {
        CGEventType::LeftMouseDown | CGEventType::RightMouseDown | CGEventType::OtherMouseDown => {
            let p = event.location();
            let button = match etype {
                CGEventType::LeftMouseDown => MouseButton::Left,
                CGEventType::RightMouseDown => MouseButton::Right,
                _ => MouseButton::Other,
            };

            // НОВОЕ: Extract modifiers
            let flags = event.get_flags();
            let modifiers = decode_modifiers(flags);

            RawEvent::MouseDown {
                button,
                x: p.x,
                y: p.y,
                modifiers,  // NEW
            }
        }
        // ...
    }
}

fn decode_modifiers(flags: CGEventFlags) -> Vec<Modifier> {
    let mut mods = Vec::new();
    if flags.contains(CGEventFlags::CGEventFlagCommand) {
        mods.push(Modifier::Command);
    }
    if flags.contains(CGEventFlags::CGEventFlagShift) {
        mods.push(Modifier::Shift);
    }
    if flags.contains(CGEventFlags::CGEventFlagControl) {
        mods.push(Modifier::Control);
    }
    if flags.contains(CGEventFlags::CGEventFlagOption) {
        mods.push(Modifier::Option);
    }
    mods
}
```

#### 3.3 RawEvent expansion

```rust
// crates/gilb-a11y/src/events.rs

#[derive(Debug, Clone)]
pub enum RawEvent {
    KeyDown {
        special: Option<SpecialKey>,
        text: Option<String>,
    },
    MouseDown {
        button: MouseButton,
        x: f64,
        y: f64,
        modifiers: Vec<Modifier>,  // NEW
    },
    Scroll {
        delta_y: i64,
        delta_x: i64,
    },
}
```

#### 3.4 Normalizer для click detail

```rust
// crates/gilb-a11y/src/normalizer.rs

struct Normalizer {
    // ...
    click_state: Arc<Mutex<ClickState>>,  // NEW
}

async fn emit_click(
    &self,
    button: MouseButton,
    x: f64,
    y: f64,
    modifiers: Vec<Modifier>,  // NEW param
    snap: &FocusSnapshot,
    drops: &mut u64,
) {
    let click_count = self.click_state.lock().register_click(button);  // NEW

    let action = Action {
        // ...
        click_count: Some(click_count),  // NEW
        modifiers: if modifiers.is_empty() { None } else { Some(modifiers) },  // NEW
        // ...
    };
    // ...
}
```

#### 3.5 Миграция БД

```sql
-- crates/gilb-db/migrations/0008_click_detail.sql

ALTER TABLE actions ADD COLUMN click_count INTEGER;
ALTER TABLE actions ADD COLUMN modifiers TEXT;  -- JSON: ["Command","Shift"]

CREATE INDEX idx_actions_clicks
    ON actions(session_id, captured_at)
    WHERE kind = 'click' AND click_count > 1;
```

---

### Phase 4: Drag & Drop (P2) — Week 4

**Цель:** Отслеживать перетаскивание элементов

#### 4.1 Новые ActionKind

```rust
// crates/gilb-core/src/lib.rs

pub enum ActionKind {
    // Существующие
    Click, Text, Key, Scroll, Clipboard, FocusChange, Debug,

    // НОВЫЕ
    DragStart,      // Начало перетаскивания
    DragEnd,        // Конец перетаскивания
}
```

#### 4.2 Drag state tracking

```rust
// crates/gilb-a11y/src/platform/macos/event_tap.rs

struct DragState {
    is_dragging: bool,
    start_x: f64,
    start_y: f64,
    start_element: Option<ElementContext>,
    start_time: Instant,
}

fn events_of_interest() -> Vec<CGEventType> {
    vec![
        CGEventType::KeyDown,
        CGEventType::LeftMouseDown,
        CGEventType::RightMouseDown,
        CGEventType::OtherMouseDown,
        CGEventType::ScrollWheel,
        // НОВЫЕ для drag
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
    ]
}

fn decode_event(etype: CGEventType, event: &CGEvent) -> Option<RawEvent> {
    match etype {
        CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged => {
            let p = event.location();
            RawEvent::MouseDrag {
                current_x: p.x,
                current_y: p.y,
            }
        }
        // ...
    }
}
```

#### 4.3 Normalizer для drag

```rust
// crates/gilb-a11y/src/normalizer.rs

async fn handle_raw(&self, ev: RawEvent, buffer: &mut TextBuffer, drops: &mut u64) {
    match ev {
        RawEvent::MouseDown { button, x, y, modifiers } => {
            // Потенциальное начало drag
            self.drag_state.lock().start(button, x, y);
            self.flush_text(buffer, FlushReason::Click).await;
            self.emit_click(button, x, y, modifiers, &snap, drops).await;
        }
        RawEvent::MouseDrag { current_x, current_y } => {
            self.drag_state.lock().update(current_x, current_y);
        }
        RawEvent::MouseUp { button, x, y } => {
            if let Some(drag) = self.drag_state.lock().complete() {
                // Это было drag & drop
                self.emit_drag_end(drag, &snap, drops).await;
            }
        }
        // ...
    }
}
```

#### 4.4 Миграция БД

```sql
-- crates/gilb-db/migrations/0009_drag_context.sql

ALTER TABLE actions ADD COLUMN drag_start_x REAL;
ALTER TABLE actions ADD COLUMN drag_start_y REAL;
ALTER TABLE actions ADD COLUMN drag_end_x REAL;
ALTER TABLE actions ADD COLUMN drag_end_y REAL;
ALTER TABLE actions ADD COLUMN drag_origin_element TEXT;  -- JSON
ALTER TABLE actions ADD COLUMN drag_destination_element TEXT;  -- JSON

CREATE INDEX idx_actions_drag
    ON actions(session_id, captured_at)
    WHERE kind IN ('drag_start', 'drag_end');
```

---

## Implementation Priority

```
Week 1: Phase 1 - Selection (P0)
├── ElementContext: selected_text, selection_range
├── AX integration: AXSelectedText, AXSelectedTextRange
├── DB migration: 0006_selection_context.sql
└── Tests

Week 2: Phase 2 - Focus Element (P1) + Phase 3 start
├── FocusSnapshot: focused_element (full context)
├── AX: AXFocusedUIElement
├── DB migration: 0007_focused_element.sql
└── Start click_count/modifiers

Week 2-3: Phase 3 - Click Detail (P1)
├── Action: click_count, modifiers
├── EventTap: CGEventFlags, click tracking
├── RawEvent: modifiers field
├── Normalizer: click state machine
└── DB migration: 0008_click_detail.sql

Week 4: Phase 4 - Drag & Drop (P2)
├── ActionKind: DragStart, DragEnd
├── EventTap: mouse dragged events
├── Normalizer: drag state machine
└── DB migration: 0009_drag_context.sql
```

---

## Success Criteria

### Phase 1 (Selection)
- [ ] `selected_text` заполняется при выделении текста
- [ ] `selection_range` корректно показывает {start, end}
- [ ] Работает в TextEdit, Safari, Chrome
- [ ] Тесты проходят

### Phase 2 (Focus Element)
- [ ] `focused_element` содержит полный ElementContext
- [ ] Текстовые события (Text) имеют корректный element context
- [ ] Secure поля правильно детектируются

### Phase 3 (Click Detail)
- [ ] `click_count` правильно детектирует double/triple click
- [ ] `modifiers` показывает Command/Shift/Control/Option
- [ ] Command+Click отличается от Click

### Phase 4 (Drag & Drop)
- [ ] DragStart событие при начале перетаскивания
- [ ] DragEnd событие с origin/destination
- [ ] Работает в Finder (файлы)

---

## Testing Strategy

### Unit Tests
```rust
// crates/gilb-core/tests/
#[test]
fn test_selection_range_serialization() {
    let range = SelectionRange { start: 0, end: 10 };
    let json = serde_json::to_string(&range).unwrap();
    assert_eq!(json, r#"{"start":0,"end":10}"#);
}

#[test]
fn test_modifier_flags() {
    let mods = vec![Modifier::Command, Modifier::Shift];
    let json = serde_json::to_string(&mods).unwrap();
    assert!(json.contains("Command"));
    assert!(json.contains("Shift"));
}
```

### Integration Tests
```rust
// crates/gilb-a11y/tests/macos/
#[tokio::test]
async fn test_selection_capture_in_safari() {
    // Симуляция выделения текста в Safari
    // Проверка что selected_text сохранён
}

#[tokio::test]
async fn test_double_click_detection() {
    // Симуляция двойного клика
    // Проверка что click_count = 2
}

#[tokio::test]
async fn test_command_click() {
    // Симуляция Command+Click
    // Проверка что modifiers содержит Command
}
```

### Migration Tests
```rust
// crates/gilb-db/tests/
#[sqlx::test]
async fn test_0006_selection_context_migration(pool: SqlitePool) {
    migrate(&pool).await.unwrap();
    
    // Проверка что колонки существуют
    let row = sqlx::query("PRAGMA table_info(actions)")
        .fetch_all(&pool)
        .await.unwrap();
    
    assert!(row.iter().any(|c| c.name == "selected_text"));
    assert!(row.iter().any(|c| c.name == "selection_range"));
}
```

---

## Rollout Plan

### Week 1
1. **Monday-Tuesday**: Selection context implementation
2. **Wednesday**: AX integration for selection
3. **Thursday**: Tests + fixes
4. **Friday**: Code review + merge

### Week 2
1. **Monday**: Focus element tracking
2. **Wednesday-Friday**: Click detail (start)

### Week 2-3
1. **Finish**: Click detail + tests

### Week 4
1. **Full week**: Drag & drop

---

## Open Questions

1. **AXSelectedTextRange формат**: CFDictionary или CFArray?
   - **Нужно исследовать**: Как macOS возвращает range

2. **Poll vs Notification для selection**: Как Codex отслеживает изменения выделения?
   - **Recommendation**: Начать с polling на focus_tick, позже AX notifications

3. **Click timeout**: Какой интервал для double-click detection?
   - **Recommendation**: 500ms (стандарт macOS)

4. **Drag detection**: MouseDragged или разница между MouseUp/Down?
   - **Recommendation**: MouseDragged events (надёжнее)

---

## References

- Codex Record & Reverse reverse analysis: `/root/src/research/codex/codex-record-replay-prompts-original.md`
- Apple AX Documentation: https://developer.apple.com/documentation/accessibility
- CGEventTap Documentation: https://developer.apple.com/documentation/coregraphics/cgeventtap
